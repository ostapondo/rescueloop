use anyhow::{Result, bail};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider},
};
use std::time::Duration;

const TRACE_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
const TRACE_QUEUE_SIZE: usize = 1024;
const TRACE_BATCH_SIZE: usize = 128;
const MAX_ENDPOINT_BYTES: usize = 2_048;

pub(super) const EXPORTABLE_SPANS: &[&str] = &[
    "observation.process",
    "incident.persist",
    "incident.group",
    "analysis.run",
    "repair.apply",
    "repair.operational",
    "verification.run",
    "rollback.run",
];

pub(super) fn is_exportable_span(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.is_span() && EXPORTABLE_SPANS.contains(&metadata.name())
}

pub fn configure() -> Result<Option<SdkTracerProvider>> {
    let endpoint = std::env::var("RESCUELOOP_OTLP_TRACES_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty());
    configure_endpoint(endpoint.as_deref())
}

fn configure_endpoint(endpoint: Option<&str>) -> Result<Option<SdkTracerProvider>> {
    let Some(endpoint) = endpoint else {
        return Ok(None);
    };
    if endpoint.len() > MAX_ENDPOINT_BYTES {
        bail!("RESCUELOOP_OTLP_TRACES_ENDPOINT is too long")
    }
    let parsed = reqwest::Url::parse(endpoint)
        .map_err(|_| anyhow::anyhow!("RESCUELOOP_OTLP_TRACES_ENDPOINT is not a valid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("RESCUELOOP_OTLP_TRACES_ENDPOINT must use http or https")
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        bail!("RESCUELOOP_OTLP_TRACES_ENDPOINT must not contain credentials or a fragment")
    }
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(TRACE_EXPORT_TIMEOUT)
        .build()?;
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(TRACE_QUEUE_SIZE)
                .with_max_export_batch_size(TRACE_BATCH_SIZE)
                .with_scheduled_delay(Duration::from_secs(5))
                .build(),
        )
        .build();
    let provider = SdkTracerProvider::builder()
        .with_resource(Resource::builder().with_service_name("rescueloop").build())
        .with_span_processor(processor)
        .build();
    Ok(Some(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_export_is_explicit_opt_in_and_validates_transport() {
        assert!(configure_endpoint(None).unwrap().is_none());
        assert!(configure_endpoint(Some("file:///tmp/traces")).is_err());
        assert!(configure_endpoint(Some("https://token@example.test/v1/traces")).is_err());
        assert!(
            configure_endpoint(Some(&format!("https://example.test/{}", "x".repeat(2_048))))
                .is_err()
        );
        assert!(
            configure_endpoint(Some("https://collector.example/v1/traces"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn only_explicit_bounded_stage_spans_are_exportable() {
        assert!(EXPORTABLE_SPANS.contains(&"analysis.run"));
        assert!(EXPORTABLE_SPANS.contains(&"verification.run"));
        assert!(!EXPORTABLE_SPANS.contains(&"analysis.http"));
        assert!(!EXPORTABLE_SPANS.contains(&"verification.replay"));
    }
}
