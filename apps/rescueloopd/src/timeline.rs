use anyhow::Result;
use chrono::{DateTime, Utc};
use rescueloop_core::{
    AnalysisId, Incident, IncidentId, IncidentStatus, ObservationId, OccurrenceId, PlanId,
    RepairTransactionId, VerificationId,
};
use rescueloop_ledger::{
    LedgerEntry, NewLedgerEntry, NewTimelineEvent, TimelineComponent, TimelineOutcome,
    TimelineTransition,
};
use serde::Serialize;
use std::path::Path;
use uuid::Uuid;

const MAX_TIMELINE_EVENTS: usize = 256;

#[derive(Clone, Debug, Serialize)]
pub struct TimelineEvent {
    pub timestamp: DateTime<Utc>,
    pub correlation_id: Uuid,
    pub observation_id: Option<ObservationId>,
    pub incident_id: Option<IncidentId>,
    pub occurrence_id: Option<OccurrenceId>,
    pub analysis_id: Option<AnalysisId>,
    pub plan_id: Option<PlanId>,
    pub repair_transaction_id: Option<RepairTransactionId>,
    pub verification_id: Option<VerificationId>,
    pub component: TimelineComponent,
    pub lifecycle_transition: TimelineTransition,
    pub outcome: TimelineOutcome,
    pub explanation: String,
    pub ledger_entry_id: Uuid,
    pub delay_or_refusal_reason: Option<String>,
}

pub struct EventSpec<'a> {
    pub correlation_id: Option<Uuid>,
    pub component: TimelineComponent,
    pub transition: TimelineTransition,
    pub outcome: TimelineOutcome,
    pub explanation: &'a str,
    pub reason: Option<&'a str>,
    pub status: IncidentStatus,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StageIdentifiers {
    pub observation_id: Option<ObservationId>,
    pub occurrence_id: Option<OccurrenceId>,
    pub analysis_id: Option<AnalysisId>,
    pub plan_id: Option<PlanId>,
    pub repair_transaction_id: Option<RepairTransactionId>,
    pub verification_id: Option<VerificationId>,
}

pub async fn record(
    incident_dir: &Path,
    incident: &Incident,
    spec: EventSpec<'_>,
) -> Result<Option<LedgerEntry>> {
    record_with_ids(incident_dir, incident, spec, StageIdentifiers::default()).await
}

pub async fn record_with_ids(
    incident_dir: &Path,
    incident: &Incident,
    spec: EventSpec<'_>,
    ids: StageIdentifiers,
) -> Result<Option<LedgerEntry>> {
    let mut timeline = NewTimelineEvent::bounded(
        spec.correlation_id
            .unwrap_or_else(|| incident.correlation_id()),
        spec.occurred_at,
        spec.component,
        spec.transition,
        spec.outcome,
        spec.explanation,
        spec.reason.map(str::to_owned),
    )?
    .with_incident_ids(incident);
    if let Some(observation_id) = ids.observation_id {
        timeline.observation_id = Some(observation_id);
    }
    if let Some(occurrence_id) = ids.occurrence_id {
        timeline.occurrence_id = Some(occurrence_id);
    }
    timeline.analysis_id = ids.analysis_id;
    timeline.plan_id = ids.plan_id;
    timeline.repair_transaction_id = ids.repair_transaction_id;
    timeline.verification_id = ids.verification_id;
    rescueloop_ledger::append_timeline_if_missing(
        &crate::incident_store::ledger_path(incident_dir),
        NewLedgerEntry {
            incident: incident.clone(),
            repair: None,
            before_state: None,
            after_state: None,
            verifier: None,
            status: spec.status,
            relation_override: None,
            timeline: Some(timeline),
        },
    )
    .await
}

pub async fn ensure_initial(incident_dir: &Path, incident: &Incident) -> Result<()> {
    for spec in [
        EventSpec {
            correlation_id: None,
            component: TimelineComponent::Detector,
            transition: TimelineTransition::Observed,
            outcome: TimelineOutcome::Completed,
            explanation: "Objective failure observation accepted",
            reason: None,
            status: IncidentStatus::Detected,
            occurred_at: incident.observed_at,
        },
        EventSpec {
            correlation_id: None,
            component: TimelineComponent::Normalizer,
            transition: TimelineTransition::Normalized,
            outcome: TimelineOutcome::Completed,
            explanation: "Failure converted to bounded normalized evidence",
            reason: None,
            status: IncidentStatus::Detected,
            occurred_at: incident.observed_at,
        },
        EventSpec {
            correlation_id: None,
            component: TimelineComponent::IncidentStore,
            transition: TimelineTransition::Persisted,
            outcome: TimelineOutcome::Completed,
            explanation: "Versioned incident JSON persisted locally",
            reason: None,
            status: IncidentStatus::Detected,
            occurred_at: Utc::now(),
        },
    ] {
        record(incident_dir, incident, spec).await?;
    }
    Ok(())
}

pub async fn load(incident_dir: &Path, incident: &Incident) -> Result<Vec<TimelineEvent>> {
    let mut events = rescueloop_ledger::load(&crate::incident_store::ledger_path(incident_dir))
        .await?
        .into_iter()
        .filter(|entry| entry.incident_id == incident.id)
        .filter_map(|entry| {
            let timeline = entry.timeline?;
            let explanation = timeline.explanation().to_owned();
            let delay_or_refusal_reason = timeline.delay_or_refusal_reason().map(str::to_owned);
            Some(TimelineEvent {
                timestamp: timeline.occurred_at,
                correlation_id: timeline.correlation_id,
                observation_id: timeline.observation_id,
                incident_id: timeline.incident_id,
                occurrence_id: timeline.occurrence_id,
                analysis_id: timeline.analysis_id,
                plan_id: timeline.plan_id,
                repair_transaction_id: timeline.repair_transaction_id,
                verification_id: timeline.verification_id,
                component: timeline.component,
                lifecycle_transition: timeline.transition,
                outcome: timeline.outcome,
                explanation,
                ledger_entry_id: entry.id,
                delay_or_refusal_reason,
            })
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.timestamp);
    if events.len() > MAX_TIMELINE_EVENTS {
        let latest = events.split_off(events.len() - (MAX_TIMELINE_EVENTS - 3));
        events.truncate(3);
        events.extend(latest);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{Evidence, IncidentKind};
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn initial_timeline_is_idempotent_and_hash_linked() {
        let root = std::env::temp_dir().join(format!("rescueloop-timeline-{}", Uuid::new_v4()));
        let incidents = root.join("incidents");
        tokio::fs::create_dir_all(&incidents).await.unwrap();
        let incident = Incident::detected(
            "test",
            IncidentKind::Crash,
            "fixture",
            Evidence {
                source: "fixture".into(),
                summary: "fixture".into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        );
        ensure_initial(&incidents, &incident).await.unwrap();
        ensure_initial(&incidents, &incident).await.unwrap();
        let events = load(&incidents, &incident).await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].lifecycle_transition, TimelineTransition::Observed);
        assert_eq!(
            events[1].lifecycle_transition,
            TimelineTransition::Normalized
        );
        assert_eq!(
            events[2].lifecycle_transition,
            TimelineTransition::Persisted
        );
        assert!(
            events
                .iter()
                .all(|event| event.correlation_id == incident.correlation_id())
        );
        assert!(events.iter().all(|event| {
            event.observation_id == Some(incident.observation_id())
                && event.incident_id == Some(incident.incident_id())
                && event.occurrence_id == Some(incident.occurrence_id())
        }));
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].ledger_entry_id != pair[1].ledger_entry_id)
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn timeline_projection_is_bounded_and_keeps_origin_events() {
        let root = std::env::temp_dir().join(format!("rescueloop-timeline-{}", Uuid::new_v4()));
        let incidents = root.join("incidents");
        tokio::fs::create_dir_all(&incidents).await.unwrap();
        let incident = Incident::detected(
            "test",
            IncidentKind::Crash,
            "fixture",
            Evidence {
                source: "fixture".into(),
                summary: "fixture".into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        );
        ensure_initial(&incidents, &incident).await.unwrap();
        for index in 0..260 {
            record(
                &incidents,
                &incident,
                EventSpec {
                    correlation_id: Some(Uuid::new_v4()),
                    component: TimelineComponent::Analyzer,
                    transition: TimelineTransition::Analyzed,
                    outcome: TimelineOutcome::Completed,
                    explanation: "Bounded repeated analysis event",
                    reason: None,
                    status: IncidentStatus::Diagnosed,
                    occurred_at: Utc::now() + chrono::Duration::milliseconds(index),
                },
            )
            .await
            .unwrap();
        }
        let events = load(&incidents, &incident).await.unwrap();
        assert_eq!(events.len(), MAX_TIMELINE_EVENTS);
        assert_eq!(events[0].lifecycle_transition, TimelineTransition::Observed);
        assert_eq!(
            events[1].lifecycle_transition,
            TimelineTransition::Normalized
        );
        assert_eq!(
            events[2].lifecycle_transition,
            TimelineTransition::Persisted
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
