use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use crate::storage;

pub const WATCH_HEALTH_SCHEMA_VERSION: u16 = 1;
const WATCH_HEALTH_FILENAME: &str = "watch-health-v1.json";
const MAX_WATCH_HEALTH_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Healthy,
    Degraded,
    Disconnected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub name: String,
    pub state: SourceState,
    pub last_success_at: Option<DateTime<Utc>>,
    pub received: u64,
    pub dropped: u64,
    pub deduplicated: u64,
    pub reconnect_count: u64,
    pub backoff_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u16,
    pub version: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub shutdown_reason: Option<String>,
    pub sources: Vec<SourceSnapshot>,
    pub received: u64,
    pub persisted: u64,
    #[serde(default)]
    pub grouped: u64,
    pub deduplicated: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    #[serde(default)]
    pub log_write_errors: u64,
    #[serde(default)]
    pub log_export_drops: u64,
}

#[derive(Default)]
struct SourceHealth {
    state: Option<SourceState>,
    last_success_at: Option<DateTime<Utc>>,
    received: u64,
    dropped: u64,
    deduplicated: u64,
    reconnect_count: u64,
    backoff_ms: u64,
}

pub struct WatchHealth {
    started_at: DateTime<Utc>,
    queue_capacity: usize,
    sources: Mutex<BTreeMap<String, SourceHealth>>,
    received: AtomicU64,
    persisted: AtomicU64,
    grouped: AtomicU64,
    deduplicated: AtomicU64,
    queue_depth: AtomicUsize,
    log_write_errors: AtomicU64,
    log_export_drops: AtomicU64,
}

impl Default for WatchHealth {
    fn default() -> Self {
        Self::new(256)
    }
}

impl WatchHealth {
    pub fn new(queue_capacity: usize) -> Self {
        Self {
            started_at: Utc::now(),
            queue_capacity,
            sources: Mutex::new(BTreeMap::new()),
            received: AtomicU64::new(0),
            persisted: AtomicU64::new(0),
            grouped: AtomicU64::new(0),
            deduplicated: AtomicU64::new(0),
            queue_depth: AtomicUsize::new(0),
            log_write_errors: AtomicU64::new(0),
            log_export_drops: AtomicU64::new(0),
        }
    }

    pub fn source_started(&self, name: &str) {
        self.update_source(name, |s| s.state = Some(SourceState::Healthy));
    }
    pub fn source_degraded(&self, name: &str, backoff_ms: u64) {
        self.update_source(name, |s| {
            s.state = Some(SourceState::Degraded);
            s.reconnect_count = s.reconnect_count.saturating_add(1);
            s.backoff_ms = backoff_ms;
        });
    }
    pub fn observation_received(&self, name: &str) {
        self.received.fetch_add(1, Ordering::Relaxed);
        self.update_source(name, |s| {
            s.state = Some(SourceState::Healthy);
            s.last_success_at = Some(Utc::now());
            s.received = s.received.saturating_add(1);
            s.backoff_ms = 0;
        });
    }
    pub fn source_stopped(&self, name: &str) {
        self.update_source(name, |s| {
            s.state = Some(SourceState::Disconnected);
            s.backoff_ms = 0;
        });
    }
    pub fn dropped(&self, name: &str) {
        self.update_source(name, |s| s.dropped = s.dropped.saturating_add(1));
    }
    pub fn persisted(&self) {
        self.persisted.fetch_add(1, Ordering::Relaxed);
    }
    pub fn grouped(&self) {
        self.grouped.fetch_add(1, Ordering::Relaxed);
    }
    pub fn deduplicated(&self, source: &str) {
        self.deduplicated.fetch_add(1, Ordering::Relaxed);
        self.update_source(source, |s| {
            s.deduplicated = s.deduplicated.saturating_add(1)
        });
    }
    pub fn queued(&self) {
        self.queue_depth.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dequeued(&self) {
        saturating_decrement(&self.queue_depth);
    }
    pub fn set_log_health(&self, write_errors: u64, export_drops: u64) {
        self.log_write_errors.store(write_errors, Ordering::Relaxed);
        self.log_export_drops.store(export_drops, Ordering::Relaxed);
    }

    pub fn snapshot(&self, shutdown_reason: Option<String>) -> Snapshot {
        let sources = self
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, s)| SourceSnapshot {
                name: name.clone(),
                state: s.state.clone().unwrap_or(SourceState::Disconnected),
                last_success_at: s.last_success_at,
                received: s.received,
                dropped: s.dropped,
                deduplicated: s.deduplicated,
                reconnect_count: s.reconnect_count,
                backoff_ms: s.backoff_ms,
            })
            .collect();
        Snapshot {
            schema_version: WATCH_HEALTH_SCHEMA_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            started_at: self.started_at,
            updated_at: Utc::now(),
            shutdown_reason,
            sources,
            received: self.received.load(Ordering::Relaxed),
            persisted: self.persisted.load(Ordering::Relaxed),
            grouped: self.grouped.load(Ordering::Relaxed),
            deduplicated: self.deduplicated.load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            queue_capacity: self.queue_capacity,
            log_write_errors: self.log_write_errors.load(Ordering::Relaxed),
            log_export_drops: self.log_export_drops.load(Ordering::Relaxed),
        }
    }

    fn update_source(&self, name: &str, update: impl FnOnce(&mut SourceHealth)) {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        update(sources.entry(name.to_owned()).or_default());
    }
}

fn saturating_decrement(value: &AtomicUsize) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_sub(1)
    });
}

pub async fn publish(incident_dir: &Path, snapshot: &Snapshot) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(snapshot)?;
    if bytes.len() as u64 > MAX_WATCH_HEALTH_BYTES {
        anyhow::bail!("watch health snapshot exceeds size limit")
    }
    storage::replace_durable(&snapshot_path(incident_dir), &bytes).await
}

pub async fn load(incident_dir: &Path) -> Result<Option<Snapshot>> {
    let path = snapshot_path(incident_dir);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if bytes.len() as u64 > MAX_WATCH_HEALTH_BYTES {
        anyhow::bail!("watch health snapshot exceeds size limit")
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid watch health snapshot: {}", path.display()))?;
    if snapshot.schema_version != WATCH_HEALTH_SCHEMA_VERSION {
        anyhow::bail!("unsupported watch health snapshot schema")
    }
    Ok(Some(snapshot))
}

fn snapshot_path(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join(WATCH_HEALTH_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::{SourceState, WatchHealth};
    #[test]
    fn tracks_source_and_queue_health() {
        let health = WatchHealth::new(256);
        health.source_started("docker");
        health.source_degraded("docker", 2_000);
        health.observation_received("docker");
        health.queued();
        health.dequeued();
        health.persisted();
        health.deduplicated("docker");
        let snapshot = health.snapshot(None);
        assert_eq!(snapshot.sources[0].state, SourceState::Healthy);
        assert_eq!(snapshot.sources[0].reconnect_count, 1);
        assert_eq!(snapshot.sources[0].backoff_ms, 0);
        assert_eq!(snapshot.received, 1);
        assert_eq!(snapshot.persisted, 1);
        assert_eq!(snapshot.deduplicated, 1);
        assert_eq!(snapshot.queue_depth, 0);
        assert_eq!(snapshot.queue_capacity, 256);
    }

    #[tokio::test]
    async fn persists_and_rejects_oversized_health_snapshots() {
        let root = std::env::temp_dir().join(format!("rescueloop-health-{}", uuid::Uuid::new_v4()));
        let incidents = root.join("incidents");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let health = WatchHealth::new(8);
        health.source_started("fixture");
        super::publish(&incidents, &health.snapshot(None))
            .await
            .unwrap();
        assert_eq!(
            super::load(&incidents)
                .await
                .unwrap()
                .unwrap()
                .queue_capacity,
            8
        );

        let path = root.join(super::WATCH_HEALTH_FILENAME);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(super::MAX_WATCH_HEALTH_BYTES + 1).unwrap();
        assert!(super::load(&incidents).await.is_err());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
