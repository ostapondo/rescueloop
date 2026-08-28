use anyhow::Result;
use chrono::Utc;
use std::{path::Path, time::Duration};

use crate::{incident_store, logging::LogGuard, observation_journal, service, watch_health};

const STALE_WATCH_HEALTH: Duration = Duration::from_secs(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    Degraded,
    Disconnected,
}

impl HealthState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Healthy => "HEALTHY",
            Self::Degraded => "DEGRADED",
            Self::Disconnected => "DISCONNECTED",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub state: HealthState,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct DoctorSnapshot {
    pub version: String,
    pub watcher_uptime_seconds: Option<u64>,
    pub last_shutdown_reason: Option<String>,
    pub checks: Vec<Check>,
    pub sources: Vec<watch_health::SourceSnapshot>,
    pub received: u64,
    pub persisted: u64,
    pub grouped: u64,
    pub deduplicated: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub journal_pending: usize,
}

pub async fn collect(incident_dir: &Path, logs: &LogGuard) -> DoctorSnapshot {
    let service = service::snapshot().await.ok();
    let watcher_result = watch_health::load(incident_dir).await;
    let watcher_snapshot_valid = watcher_result.is_ok();
    let watcher = watcher_result.ok().flatten();
    let watcher_fresh = watcher.as_ref().is_some_and(|snapshot| {
        Utc::now()
            .signed_duration_since(snapshot.updated_at)
            .to_std()
            .is_ok_and(|age| age <= STALE_WATCH_HEALTH)
    });
    let watcher_running = service.is_some_and(|value| value.running);
    let watcher_state = if !watcher_snapshot_valid {
        HealthState::Degraded
    } else if watcher_running && watcher_fresh {
        HealthState::Healthy
    } else if watcher_running || watcher.is_some() {
        HealthState::Degraded
    } else {
        HealthState::Disconnected
    };

    let incidents = incident_store::incidents_read_only(incident_dir).await;
    let incident_count = incidents.as_ref().map_or(0, Vec::len);
    let journal = observation_journal::pending(incident_dir).await;
    let journal_pending = journal.as_ref().map_or(0, Vec::len);
    let index = incident_store::incident_index(incident_dir).await;
    let index_count = match &index {
        Ok(index) => index.count().await.ok(),
        Err(_) => None,
    };
    let ledger = rescueloop_ledger::load(&incident_store::ledger_path(incident_dir)).await;
    let background_log_errors = watcher
        .as_ref()
        .map_or(logs.write_errors(), |value| value.log_write_errors);
    let background_export_drops = watcher
        .as_ref()
        .map_or(logs.export_drops(), |value| value.log_export_drops);

    let mut checks = vec![
        Check {
            name: "watcher".into(),
            state: watcher_state,
            detail: if !watcher_snapshot_valid {
                "persisted health snapshot is invalid".into()
            } else {
                match service {
                    Some(value) if value.running => "native service is running".into(),
                    Some(value) if value.installed => {
                        "native service is installed but stopped".into()
                    }
                    Some(_) => "native service is not installed".into(),
                    None => "native service status is unavailable".into(),
                }
            },
        },
        Check {
            name: "incident store".into(),
            state: if incidents.is_ok() {
                HealthState::Healthy
            } else {
                HealthState::Degraded
            },
            detail: if incidents.is_ok() {
                format!("{incident_count} readable incident(s)")
            } else {
                "bounded incident scan failed".into()
            },
        },
        Check {
            name: "SQLite projection".into(),
            state: if index_count.is_some() {
                HealthState::Healthy
            } else {
                HealthState::Degraded
            },
            detail: index_count.map_or_else(
                || "projection could not be opened or rebuilt".into(),
                |count| format!("{count} indexed incident(s)"),
            ),
        },
        Check {
            name: "lineage ledger".into(),
            state: if ledger.is_ok() {
                HealthState::Healthy
            } else {
                HealthState::Degraded
            },
            detail: ledger.as_ref().map_or_else(
                |_| "hash-chain validation failed".into(),
                |entries| format!("{} verified entry/entries", entries.len()),
            ),
        },
        Check {
            name: "log writer".into(),
            state: if background_log_errors == 0 {
                HealthState::Healthy
            } else {
                HealthState::Degraded
            },
            detail: format!(
                "{background_log_errors} background write error(s), {background_export_drops} bounded export drop(s)"
            ),
        },
        Check {
            name: "observation journal".into(),
            state: if journal.is_ok() && journal_pending < 16 {
                HealthState::Healthy
            } else {
                HealthState::Degraded
            },
            detail: if journal.is_ok() {
                format!("{journal_pending} pending transaction(s)")
            } else {
                "journal validation failed".into()
            },
        },
    ];

    let sources = effective_sources(watcher.as_ref(), watcher_fresh, watcher_running);
    checks.sort_by(|left, right| left.name.cmp(&right.name));

    DoctorSnapshot {
        version: env!("CARGO_PKG_VERSION").into(),
        watcher_uptime_seconds: watcher.as_ref().and_then(|snapshot| {
            Utc::now()
                .signed_duration_since(snapshot.started_at)
                .to_std()
                .ok()
                .map(|duration| duration.as_secs())
        }),
        last_shutdown_reason: effective_shutdown_reason(watcher.as_ref(), watcher_running),
        checks,
        sources,
        received: watcher.as_ref().map_or(0, |value| value.received),
        persisted: watcher.as_ref().map_or(0, |value| value.persisted),
        grouped: watcher.as_ref().map_or(0, |value| value.grouped),
        deduplicated: watcher.as_ref().map_or(0, |value| value.deduplicated),
        queue_depth: watcher.as_ref().map_or(0, |value| value.queue_depth),
        queue_capacity: watcher.as_ref().map_or(0, |value| value.queue_capacity),
        journal_pending,
    }
}

fn effective_sources(
    snapshot: Option<&watch_health::Snapshot>,
    fresh: bool,
    running: bool,
) -> Vec<watch_health::SourceSnapshot> {
    let mut sources = snapshot.map_or_else(Vec::new, |value| value.sources.clone());
    if !fresh || !running {
        for source in &mut sources {
            source.state = watch_health::SourceState::Disconnected;
        }
    }
    sources
}

fn effective_shutdown_reason(
    snapshot: Option<&watch_health::Snapshot>,
    running: bool,
) -> Option<String> {
    snapshot.and_then(|snapshot| {
        snapshot
            .shutdown_reason
            .clone()
            .or_else(|| {
                running
                    .then(|| snapshot.last_shutdown_reason.clone())
                    .flatten()
            })
            .or_else(|| (!running).then(|| "abnormal_or_interrupted".into()))
    })
}

pub async fn run(incident_dir: &Path, logs: &LogGuard) -> Result<()> {
    let snapshot = collect(incident_dir, logs).await;
    println!("RescueLoop {} self-health", snapshot.version);
    println!("\nCOMPONENTS");
    for check in &snapshot.checks {
        println!(
            "{:<14} {:<12} {}",
            check.name,
            check.state.label(),
            check.detail
        );
    }
    println!("\nEVENT SOURCES");
    if snapshot.sources.is_empty() {
        println!("No watcher health snapshot is available yet.");
    }
    for source in &snapshot.sources {
        println!(
            "{:<24} {:<12} received={} dropped={} deduplicated={} reconnects={} backoff={}ms last_success={}",
            source.name,
            format!("{:?}", source.state).to_uppercase(),
            source.received,
            source.dropped,
            source.deduplicated,
            source.reconnect_count,
            source.backoff_ms,
            source
                .last_success_at
                .map_or_else(|| "never".into(), |value| value.to_rfc3339())
        );
    }
    println!("\nPIPELINE");
    println!(
        "received={} persisted={} grouped={} deduplicated={}",
        snapshot.received, snapshot.persisted, snapshot.grouped, snapshot.deduplicated
    );
    println!(
        "queue={}/{} journal_pending={}",
        snapshot.queue_depth, snapshot.queue_capacity, snapshot.journal_pending
    );
    println!(
        "uptime={} last_shutdown={}",
        snapshot
            .watcher_uptime_seconds
            .map_or_else(|| "unknown".into(), |value| format!("{value}s")),
        snapshot
            .last_shutdown_reason
            .as_deref()
            .unwrap_or("none recorded")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{effective_shutdown_reason, effective_sources};
    use crate::watch_health::{Snapshot, SourceSnapshot, SourceState, WATCH_HEALTH_SCHEMA_VERSION};
    use chrono::Utc;

    fn snapshot() -> Snapshot {
        Snapshot {
            schema_version: WATCH_HEALTH_SCHEMA_VERSION,
            version: "fixture".into(),
            pid: 1,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            shutdown_reason: None,
            last_shutdown_reason: Some("clean_shutdown".into()),
            sources: vec![SourceSnapshot {
                name: "fixture".into(),
                state: SourceState::Healthy,
                last_success_at: Some(Utc::now()),
                received: 1,
                dropped: 0,
                deduplicated: 0,
                reconnect_count: 0,
                backoff_ms: 0,
            }],
            received: 1,
            persisted: 1,
            grouped: 0,
            deduplicated: 0,
            queue_depth: 0,
            queue_capacity: 8,
            log_write_errors: 0,
            log_export_drops: 0,
        }
    }

    #[test]
    fn stale_sources_disconnect_and_unclosed_runtime_is_abnormal() {
        let snapshot = snapshot();
        assert_eq!(
            effective_sources(Some(&snapshot), false, true)[0].state,
            SourceState::Disconnected
        );
        assert_eq!(
            effective_shutdown_reason(Some(&snapshot), false).as_deref(),
            Some("abnormal_or_interrupted")
        );
        assert_eq!(
            effective_shutdown_reason(Some(&snapshot), true).as_deref(),
            Some("clean_shutdown")
        );
    }
}
