use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

mod export;
mod fallback;
mod query;
mod traces;
mod writer;

pub use query::{LogOutput, LogQuery, run as query};
pub(crate) use writer::LogHealth;
use writer::{RollingWriter, WriterConfig};

pub(crate) fn redaction_probe() -> (usize, usize) {
    writer::redaction_probe()
}

const DEFAULT_FILTER: &str = "info,hyper=warn,reqwest=warn,rustls=warn";
const DEFAULT_RETENTION_DAYS: usize = 14;
const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

pub struct LogGuard {
    health: LogHealth,
    exporter: Option<tokio::task::JoinHandle<()>>,
    tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

pub fn init(incident_dir: &Path) -> Result<LogGuard> {
    let directory = log_directory(incident_dir);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("cannot create log directory: {}", directory.display()))?;
    let retention_days = retention_days();
    let export = export::configure(&directory)?;
    let tracer_provider = traces::configure()?;
    let config = WriterConfig {
        directory: directory.clone(),
        max_file_bytes: max_file_bytes(),
        retention_days,
        compress_rotated: true,
        run_id: uuid::Uuid::new_v4().to_string(),
        export: export.as_ref().map(|value| value.sink.clone()),
    };
    let appender = RollingWriter::new(config)?;
    let health = appender.health();
    let filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(EnvFilter::new)
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_FILTER));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(appender)
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_filter(filter);
    use opentelemetry::trace::TracerProvider as _;
    let trace_layer = tracer_provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("rescueloop"))
            // Logs keep their existing redaction path. Only explicitly reviewed lifecycle spans
            // are exported, and tracing events (including error text) never become span events.
            .with_filter(tracing_subscriber::filter::filter_fn(
                traces::is_exportable_span,
            ))
    });
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(trace_layer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("cannot initialize operational logging: {error}"))?;

    install_panic_hook();
    tracing::info!(
        event = "logging.initialized",
        directory = %directory.display(),
        retention_days,
        format = "jsonl",
        schema_version = 1,
        "Operational logging initialized"
    );
    let exporter = export.map(export::spawn);
    Ok(LogGuard {
        health,
        exporter,
        tracer_provider,
    })
}

impl LogGuard {
    pub fn health(&self) -> LogHealth {
        self.health.clone()
    }

    pub fn write_errors(&self) -> u64 {
        self.health.write_errors()
    }

    pub fn export_drops(&self) -> u64 {
        self.health.export_drops()
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        if let Some(exporter) = self.exporter.take() {
            exporter.abort();
        }
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

pub fn log_directory(incident_dir: &Path) -> PathBuf {
    incident_dir.parent().unwrap_or(incident_dir).join("logs")
}

fn retention_days() -> usize {
    std::env::var("RESCUELOOP_LOG_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

fn max_file_bytes() -> u64 {
    std::env::var("RESCUELOOP_LOG_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value >= 1024)
        .unwrap_or(DEFAULT_MAX_FILE_BYTES)
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        fallback::emergency(&format!("RescueLoop panic: {panic}"));
        tracing::error!(
            event = "runtime.panic",
            panic = %panic,
            "RescueLoop panicked"
        );
        previous(panic);
    }));
}

#[cfg(debug_assertions)]
pub fn trigger_test_panic_if_requested() {
    if std::env::var("RESCUELOOP_TEST_PANIC").as_deref() == Ok("1") {
        panic!("requested debug panic for logging validation");
    }
}

#[cfg(not(debug_assertions))]
pub fn trigger_test_panic_if_requested() {}

#[cfg(test)]
mod tests {
    use super::log_directory;
    use std::path::Path;

    #[test]
    fn stores_logs_next_to_incident_state() {
        assert_eq!(
            log_directory(Path::new("state/incidents")),
            Path::new("state/logs")
        );
    }
}
