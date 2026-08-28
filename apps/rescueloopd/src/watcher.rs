use anyhow::Result;
use rescueloop_core::{Incident, IncidentCollector};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
    console::load_settings,
    incident_store::{SaveOutcome, recover_pending_observations, save_incident},
    metrics::{DropReason, EventSource, registry},
    watch_health::{self, WatchHealth},
};

const EVENT_QUEUE_CAPACITY: usize = 256;
pub(crate) const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(30);

pub async fn run(directory: &Path, log_health: crate::logging::LogHealth) -> Result<()> {
    tokio::fs::create_dir_all(directory).await?;
    let recovered = recover_pending_observations(directory).await?;
    if recovered > 0 {
        info!(
            event = "watch.recovery_completed",
            recovered, "Interrupted observation transactions recovered"
        );
    }
    let settings = load_settings(directory).await?;
    let sources = rescueloop_platform::event_sources(&settings.enabled_sources)?;
    let source_names = sources
        .iter()
        .map(|source| source.name())
        .collect::<Vec<_>>();
    announce(directory, &source_names);

    let (sender, mut events) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let health = Arc::new(WatchHealth::new(EVENT_QUEUE_CAPACITY));
    let previous_snapshot = watch_health::load(directory).await;
    let previous_shutdown = match &previous_snapshot {
        Ok(Some(snapshot)) => Some(
            snapshot
                .shutdown_reason
                .clone()
                .unwrap_or_else(|| "abnormal_or_interrupted".into()),
        ),
        Ok(None) => None,
        Err(_) => {
            warn!(
                event = "watch.health_snapshot_invalid",
                reason = "invalid_or_oversized",
                "Invalid disposable health snapshot will be replaced"
            );
            Some("health_snapshot_invalid".into())
        }
    };
    health.set_last_shutdown_reason(previous_shutdown);
    health.set_last_shutdown_duration_ms(
        previous_snapshot
            .ok()
            .flatten()
            .and_then(|snapshot| snapshot.last_shutdown_duration_ms),
    );
    health.set_log_health(log_health.write_errors(), log_health.export_drops());
    registry().set_log_write_failures(log_health.write_errors());
    health.publish_to(directory, None).await?;
    let cancellation = CancellationToken::new();
    let mut tasks = JoinSet::new();
    spawn_heartbeat(
        &mut tasks,
        source_names.len(),
        directory.to_path_buf(),
        Arc::clone(&health),
        cancellation.clone(),
        log_health.clone(),
    );
    for source in sources {
        tasks.spawn(run_source(
            source,
            sender.clone(),
            Arc::clone(&health),
            cancellation.clone(),
            directory.to_path_buf(),
        ));
    }
    drop(sender);

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let outcome: Result<bool> = loop {
        tokio::select! {
            signal = &mut shutdown => {
                match signal {
                    Ok(()) => {
                        info!(event = "watch.shutdown_requested", "Watcher shutdown requested");
                        break Ok(false);
                    }
                    Err(error) => break Err(error),
                }
            }
            event = events.recv() => match event {
                Some((source, incident)) => {
                    if let Err(error) = persist(directory, &source, incident, &health).await {
                        break Err(error);
                    }
                }
                None => break Ok(true),
            },
            task = tasks.join_next() => match task {
                Some(Ok(())) => break Err(anyhow::anyhow!("observation worker stopped unexpectedly")),
                Some(Err(error)) => break Err(error.into()),
                None => break Err(anyhow::anyhow!("observation worker set became empty")),
            },
        }
    };

    cancellation.cancel();
    let (exhausted, mut failure) = match outcome {
        Ok(exhausted) => (exhausted, None),
        Err(error) => (false, Some(error)),
    };
    let shutdown_started = std::time::Instant::now();
    if failure.is_none() {
        let shutdown = async {
            while let Some((source, incident)) = events.recv().await {
                persist(directory, &source, incident, &health).await?;
            }
            while let Some(result) = tasks.join_next().await {
                result?;
            }
            Ok::<_, anyhow::Error>(())
        };
        match tokio::time::timeout(SHUTDOWN_DEADLINE, shutdown).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failure = Some(error),
            Err(_) => {
                tasks.abort_all();
                warn!(
                    event = "watch.shutdown_deadline_exceeded",
                    queue_depth = health.snapshot(None).queue_depth,
                    "Watcher shutdown deadline exceeded"
                );
                failure = Some(anyhow::anyhow!("watcher shutdown deadline exceeded"));
            }
        }
    } else {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    let shutdown_duration_ms = shutdown_started.elapsed().as_millis() as u64;
    health.set_last_shutdown_duration_ms(Some(shutdown_duration_ms));

    if shutdown_duration_ms > SHUTDOWN_DEADLINE.as_millis() as u64 && failure.is_none() {
        failure = Some(anyhow::anyhow!("watcher shutdown deadline exceeded"));
    }

    if let Some(error) = failure {
        let _ = health.publish_to(directory, Some("failure".into())).await;
        return Err(error);
    }

    if exhausted {
        error!(
            event = "watch.sources_exhausted",
            "All event sources stopped"
        );
        let _ = health
            .publish_to(directory, Some("sources_exhausted".into()))
            .await;
        anyhow::bail!("all event sources stopped")
    }
    health
        .publish_to(directory, Some("clean_shutdown".into()))
        .await?;
    info!(event = "watch.stopped", "Watcher stopped cleanly");
    Ok(())
}

fn announce(directory: &Path, sources: &[&str]) {
    info!(event = "watch.ready", sources = ?sources, "Watcher initialized");
    println!("RescueLoop {}", env!("CARGO_PKG_VERSION"));
    println!("Status: READY — monitoring for objective failures");
    println!(
        "Platform: {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("Event sources: {}", sources.join(", "));
    println!("Incidents: {}", directory.display());
    println!("Privacy: local detection only; AI analysis starts only on request");
    println!("Waiting for a new failure event...\n");
}

fn spawn_heartbeat(
    tasks: &mut JoinSet<()>,
    source_count: usize,
    incident_dir: std::path::PathBuf,
    health: Arc<WatchHealth>,
    cancellation: CancellationToken,
    log_health: crate::logging::LogHealth,
) {
    tasks.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    health.set_log_health(log_health.write_errors(), log_health.export_drops());
                    registry().set_log_write_failures(log_health.write_errors());
                    if let Err(error) = health.publish_to(&incident_dir, None).await {
                        warn!(event = "watch.health_publish_failed", error = %format!("{error:#}"), "Watcher health snapshot could not be published");
                    }
                    let snapshot = health.snapshot(None);
                    info!(
                        event = "watch.heartbeat",
                        source_count,
                        active_sources = snapshot.sources.iter().filter(|source| source.state != crate::watch_health::SourceState::Disconnected).count(),
                        degraded_sources = snapshot.sources.iter().filter(|source| source.state == crate::watch_health::SourceState::Degraded).count(),
                        retry_count = snapshot.sources.iter().map(|source| source.reconnect_count).sum::<u64>(),
                        received = snapshot.received,
                        persisted = snapshot.persisted,
                        deduplicated = snapshot.deduplicated,
                        queue_depth = snapshot.queue_depth,
                        "Watcher is alive"
                    );
                }
            }
        }
    });
}

async fn run_source(
    mut source: Box<dyn IncidentCollector>,
    sender: mpsc::Sender<(String, Incident)>,
    health: Arc<WatchHealth>,
    cancellation: CancellationToken,
    incident_dir: std::path::PathBuf,
) {
    let source_name = source.name().to_owned();
    health.source_started(&source_name);
    let _ = health.publish_to(&incident_dir, None).await;
    info!(
        event = "source.started",
        source = source.name(),
        "Event source started"
    );
    let mut retry_delay = initial_retry_delay();
    let mut degraded = false;
    loop {
        let result = tokio::select! {
            _ = cancellation.cancelled() => break,
            result = source.next_incident() => result,
        };
        match result {
            Ok(incident) => {
                if degraded {
                    info!(
                        event = "source.recovered",
                        source = source.name(),
                        "Event source recovered"
                    );
                    degraded = false;
                }
                retry_delay = initial_retry_delay();
                info!(event = "observation.received", source = source.name(), observation_id = %incident.observation_id(), incident_id = %incident.incident_id(), occurrence_id = %incident.occurrence_id(), kind = ?incident.kind, "Failure observation received");
                health.observation_received(&source_name);
                registry().event_received(EventSource::from_name(&source_name));
                if let Err(error) =
                    crate::observation_journal::begin(&incident_dir, &incident).await
                {
                    health.dropped(&source_name);
                    registry().event_dropped(DropReason::PersistenceFailed);
                    error!(
                        event = "observation.journal_failed",
                        source = source.name(),
                        error = %format!("{error:#}"),
                        "Observation was not accepted because durable journaling failed"
                    );
                    break;
                }
                let permit = tokio::select! {
                    _ = cancellation.cancelled() => {
                        registry().event_dropped(DropReason::Shutdown);
                        None
                    },
                    result = sender.reserve() => match result {
                        Ok(permit) => Some(permit),
                        Err(_) => {
                            registry().event_dropped(DropReason::QueueClosed);
                            None
                        }
                    },
                };
                let Some(permit) = permit else {
                    health.dropped(&source_name);
                    break;
                };
                health.queued();
                health.observation_accepted();
                registry().set_queue_depth(health.snapshot(None).queue_depth);
                permit.send((source_name.clone(), incident));
            }
            Err(error) => {
                degraded = true;
                health.source_degraded(&source_name, retry_delay.as_millis() as u64);
                registry().source_reconnected();
                let _ = health.publish_to(&incident_dir, None).await;
                warn!(event = "source.retrying", source = source.name(), error = %format!("{error:#}"), retry_delay_ms = retry_delay.as_millis(), "Event source failed; reconnecting");
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(retry_delay) => {}
                }
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
            }
        }
    }
    health.source_stopped(&source_name);
    let _ = health.publish_to(&incident_dir, None).await;
    info!(
        event = "source.stopped",
        source = source.name(),
        reason = "shutdown",
        "Event source stopped"
    );
}

fn initial_retry_delay() -> Duration {
    #[cfg(test)]
    return Duration::from_millis(10);
    #[cfg(not(test))]
    Duration::from_secs(2)
}

async fn persist(
    directory: &Path,
    source: &str,
    incident: Incident,
    health: &WatchHealth,
) -> Result<()> {
    health.dequeued();
    registry().set_queue_depth(health.snapshot(None).queue_depth);
    let (destination, outcome) = match save_incident(directory, &incident).await {
        Ok(result) => result,
        Err(error) => {
            registry().event_dropped(DropReason::PersistenceFailed);
            return Err(error);
        }
    };
    match outcome {
        SaveOutcome::Duplicate => {
            health.deduplicated(source);
            health.publish_to(directory, None).await?;
            info!(event = "incident.duplicate", observation_id = %incident.observation_id(), incident_id = %incident.incident_id(), occurrence_id = %incident.occurrence_id(), "Exact duplicate observation ignored");
            return Ok(());
        }
        SaveOutcome::Grouped => {
            health.grouped();
            health.publish_to(directory, None).await?;
            info!(event = "incident.grouped", observation_id = %incident.observation_id(), incident_id = %incident.incident_id(), occurrence_id = %incident.occurrence_id(), "Incident grouped with an active failure");
            return Ok(());
        }
        SaveOutcome::Created => {}
    }
    health.persisted();
    health.publish_to(directory, None).await?;
    info!(event = "incident.persisted", observation_id = %incident.observation_id(), incident_id = %incident.incident_id(), occurrence_id = %incident.occurrence_id(), kind = ?incident.kind, "New incident persisted");
    println!("DETECTED: {:?}: {}", incident.kind, incident.message);
    println!("Incident saved to {}", destination.display());
    println!(
        "Analysis has NOT started. Run: rescueloop analyze '{}' --endpoint <URL>",
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod source_runtime_tests {
    use super::*;
    use crate::observation_journal;
    use async_trait::async_trait;
    use rescueloop_core::{Evidence, IncidentKind};
    use std::collections::{BTreeMap, VecDeque};

    struct ScriptedSource {
        outcomes: VecDeque<anyhow::Result<Incident>>,
    }

    #[async_trait]
    impl IncidentCollector for ScriptedSource {
        fn name(&self) -> &str {
            "scripted"
        }

        async fn next_incident(&mut self) -> anyhow::Result<Incident> {
            self.outcomes
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("disconnected")))
        }
    }

    fn incident(message: &str) -> Incident {
        Incident::detected(
            "scripted",
            IncidentKind::Crash,
            message,
            Evidence {
                source: "scripted".into(),
                summary: message.into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        )
    }

    #[tokio::test]
    async fn disconnected_source_reconnects_without_losing_the_next_observation() {
        let root = std::env::temp_dir().join(format!("rescueloop-source-{}", uuid::Uuid::new_v4()));
        let incidents = root.join("incidents");
        let source = ScriptedSource {
            outcomes: VecDeque::from([
                Err(anyhow::anyhow!("temporary disconnect")),
                Ok(incident("recovered")),
            ]),
        };
        let (sender, mut receiver) = mpsc::channel(1);
        let health = Arc::new(WatchHealth::new(1));
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_source(
            Box::new(source),
            sender,
            Arc::clone(&health),
            cancellation.clone(),
            incidents.clone(),
        ));

        let (_, received) = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.message, "recovered");
        let snapshot = health.snapshot(None);
        assert!(snapshot.sources[0].reconnect_count >= 1);
        assert_eq!(snapshot.received, 1);
        assert_eq!(snapshot.accepted, 1);
        assert_eq!(
            observation_journal::pending(&incidents)
                .await
                .unwrap()
                .len(),
            1
        );

        cancellation.cancel();
        task.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn full_queue_keeps_every_received_observation_in_the_durable_journal() {
        let root = std::env::temp_dir().join(format!("rescueloop-queue-{}", uuid::Uuid::new_v4()));
        let incidents = root.join("incidents");
        let source = ScriptedSource {
            outcomes: VecDeque::from([Ok(incident("first")), Ok(incident("second"))]),
        };
        let (sender, _receiver) = mpsc::channel(1);
        let health = Arc::new(WatchHealth::new(1));
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_source(
            Box::new(source),
            sender,
            Arc::clone(&health),
            cancellation.clone(),
            incidents.clone(),
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if health.snapshot(None).received >= 2
                    && observation_journal::pending(&incidents)
                        .await
                        .is_ok_and(|pending| pending.len() == 2)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let snapshot = health.snapshot(None);
        assert_eq!(snapshot.queue_depth, 1);
        assert_eq!(snapshot.queue_capacity, 1);
        assert_eq!(snapshot.accepted, 1);
        assert_eq!(
            observation_journal::pending(&incidents)
                .await
                .unwrap()
                .len(),
            2
        );

        cancellation.cancel();
        task.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::windows;

    let mut ctrl_c = windows::ctrl_c()?;
    let mut ctrl_break = windows::ctrl_break()?;
    let mut close = windows::ctrl_close()?;
    let mut logoff = windows::ctrl_logoff()?;
    let mut shutdown = windows::ctrl_shutdown()?;
    tokio::select! {
        _ = ctrl_c.recv() => {},
        _ = ctrl_break.recv() => {},
        _ = close.recv() => {},
        _ = logoff.recv() => {},
        _ = shutdown.recv() => {},
    }
    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rescueloop_core::{Evidence, IncidentKind};
    use std::collections::BTreeMap;

    struct PendingSource;
    struct BurstSource;

    #[async_trait]
    impl IncidentCollector for PendingSource {
        fn name(&self) -> &str {
            "pending-test"
        }

        async fn next_incident(&mut self) -> Result<Incident> {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl IncidentCollector for BurstSource {
        fn name(&self) -> &str {
            "burst-test"
        }

        async fn next_incident(&mut self) -> Result<Incident> {
            Ok(Incident::detected(
                "test",
                IncidentKind::Crash,
                "failure",
                Evidence {
                    source: "test".into(),
                    summary: "failure".into(),
                    artifact: None,
                    fields: BTreeMap::new(),
                },
            ))
        }
    }

    #[tokio::test]
    async fn cancellation_stops_idle_source_without_leaking_task() {
        let (sender, _events) = mpsc::channel(1);
        let health = Arc::new(WatchHealth::default());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_source(
            Box::new(PendingSource),
            sender,
            Arc::clone(&health),
            cancellation.clone(),
            std::env::temp_dir()
                .join(format!("rescueloop-source-health-{}", uuid::Uuid::new_v4()))
                .join("incidents"),
        ));
        tokio::task::yield_now().await;
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("source task did not stop")
            .unwrap();
        assert_eq!(
            health.snapshot(None).sources[0].state,
            crate::watch_health::SourceState::Disconnected
        );
    }

    #[tokio::test]
    async fn cancellation_releases_source_blocked_by_backpressure() {
        let (sender, _events) = mpsc::channel(1);
        let health = Arc::new(WatchHealth::default());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_source(
            Box::new(BurstSource),
            sender,
            Arc::clone(&health),
            cancellation.clone(),
            std::env::temp_dir()
                .join(format!("rescueloop-source-health-{}", uuid::Uuid::new_v4()))
                .join("incidents"),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while health.snapshot(None).queue_depth != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("source did not fill the bounded queue");
        assert_eq!(health.snapshot(None).queue_depth, 1);
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("backpressured source did not stop")
            .unwrap();
        assert_eq!(
            health.snapshot(None).sources[0].state,
            crate::watch_health::SourceState::Disconnected
        );
    }
}
