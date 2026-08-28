use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Docker,
    MacosDiagnosticReports,
    MacosUnifiedLog,
    Podman,
    WindowsErrorReporting,
    WindowsEventLog,
    Other,
}

impl EventSource {
    pub fn from_name(name: &str) -> Self {
        match name {
            "docker" => Self::Docker,
            "macos-diagnostic-reports" => Self::MacosDiagnosticReports,
            "macos-unified-log" => Self::MacosUnifiedLog,
            "podman" => Self::Podman,
            "windows-error-reporting" => Self::WindowsErrorReporting,
            "windows-event-log" => Self::WindowsEventLog,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    QueueClosed,
    Shutdown,
    PersistenceFailed,
}

#[derive(Clone, Copy, Debug)]
pub enum DurationKind {
    IncidentPersist,
    IncidentGrouping,
    Analysis,
    Repair,
    Verification,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DurationSnapshot {
    pub count: u64,
    pub total_micros: u64,
    pub max_micros: u64,
    pub last_micros: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub events_received_total: BTreeMap<EventSource, u64>,
    pub events_dropped_total: BTreeMap<DropReason, u64>,
    pub source_reconnects_total: u64,
    pub queue_depth: u64,
    pub incident_persist_duration: DurationSnapshot,
    pub incident_grouping_duration: DurationSnapshot,
    pub analysis_duration: DurationSnapshot,
    pub repair_duration: DurationSnapshot,
    pub verification_duration: DurationSnapshot,
    pub rollback_total: u64,
    pub log_write_failures_total: u64,
    pub index_rebuild_total: u64,
    pub journal_pending_count: u64,
}

#[derive(Default)]
pub struct Registry {
    values: Mutex<MetricsSnapshot>,
}

impl Registry {
    pub fn event_received(&self, source: EventSource) {
        self.update(|metrics| increment(metrics.events_received_total.entry(source).or_default()));
    }

    pub fn event_dropped(&self, reason: DropReason) {
        self.update(|metrics| increment(metrics.events_dropped_total.entry(reason).or_default()));
    }

    pub fn source_reconnected(&self) {
        self.update(|metrics| increment(&mut metrics.source_reconnects_total));
    }

    pub fn set_queue_depth(&self, depth: usize) {
        self.update(|metrics| metrics.queue_depth = depth as u64);
    }

    pub fn set_journal_pending_count(&self, count: usize) {
        self.update(|metrics| metrics.journal_pending_count = count as u64);
    }

    pub fn rollback(&self) {
        self.update(|metrics| increment(&mut metrics.rollback_total));
    }

    pub fn set_log_write_failures(&self, count: u64) {
        self.update(|metrics| metrics.log_write_failures_total = count);
    }

    pub fn index_rebuilt(&self) {
        self.update(|metrics| increment(&mut metrics.index_rebuild_total));
    }

    pub fn timer(&self, kind: DurationKind) -> Timer<'_> {
        Timer {
            registry: self,
            kind,
            started: Instant::now(),
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        self.values
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn observe(&self, kind: DurationKind, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.update(|metrics| {
            let target = match kind {
                DurationKind::IncidentPersist => &mut metrics.incident_persist_duration,
                DurationKind::IncidentGrouping => &mut metrics.incident_grouping_duration,
                DurationKind::Analysis => &mut metrics.analysis_duration,
                DurationKind::Repair => &mut metrics.repair_duration,
                DurationKind::Verification => &mut metrics.verification_duration,
            };
            increment(&mut target.count);
            target.total_micros = target.total_micros.saturating_add(micros);
            target.max_micros = target.max_micros.max(micros);
            target.last_micros = micros;
        });
    }

    fn update(&self, update: impl FnOnce(&mut MetricsSnapshot)) {
        update(
            &mut self
                .values
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
    }
}

pub struct Timer<'a> {
    registry: &'a Registry,
    kind: DurationKind,
    started: Instant,
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        self.registry.observe(self.kind, self.started.elapsed());
    }
}

pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

fn increment(value: &mut u64) {
    *value = value.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_bounded_typed_labels_and_saturating_values() {
        let registry = Registry::default();
        registry.event_received(EventSource::from_name("untrusted-source-name"));
        registry.event_dropped(DropReason::QueueClosed);
        registry.source_reconnected();
        registry.set_queue_depth(7);
        registry.set_journal_pending_count(3);
        registry.rollback();
        registry.set_log_write_failures(2);
        registry.index_rebuilt();

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.events_received_total[&EventSource::Other], 1);
        assert_eq!(snapshot.events_dropped_total[&DropReason::QueueClosed], 1);
        assert_eq!(snapshot.source_reconnects_total, 1);
        assert_eq!(snapshot.queue_depth, 7);
        assert_eq!(snapshot.journal_pending_count, 3);
        assert_eq!(snapshot.rollback_total, 1);
        assert_eq!(snapshot.log_write_failures_total, 2);
        assert_eq!(snapshot.index_rebuild_total, 1);
    }

    #[test]
    fn duration_summary_is_bounded() {
        let registry = Registry::default();
        {
            let _timer = registry.timer(DurationKind::Analysis);
            std::thread::sleep(Duration::from_millis(1));
        }
        let duration = registry.snapshot().analysis_duration;
        assert_eq!(duration.count, 1);
        assert!(duration.total_micros >= 1_000);
        assert_eq!(duration.total_micros, duration.max_micros);
        assert_eq!(duration.max_micros, duration.last_micros);
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let registry = std::sync::Arc::new(Registry::default());
        let workers = (0..8)
            .map(|_| {
                let registry = std::sync::Arc::clone(&registry);
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        registry.event_received(EventSource::Docker);
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            registry.snapshot().events_received_total[&EventSource::Docker],
            8_000
        );
    }
}
