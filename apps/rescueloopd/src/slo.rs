use rescueloop_core::{AnalysisRequest, Evidence, Incident, IncidentKind, IncidentStatus};
use rescueloop_ledger::{LedgerEntry, TimelineOutcome, TimelineTransition};
use serde::Serialize;
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};

use crate::watch_health::{RUNTIME_CONTRACT_VERSION, Snapshot};

const MAX_REPAIR_RECEIPTS: usize = 256;
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionStatus {
    Pass,
    Fail,
    Unknown,
}

impl AssertionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionKind {
    ObservationDurability,
    SourceIsolation,
    QueueBounded,
    ShutdownDeadline,
    IndexRebuildable,
    VerificationIntegrity,
    RepairLedgerCoverage,
    RedactionNegativeTests,
}

impl AssertionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ObservationDurability => "observation durability",
            Self::SourceIsolation => "event source isolation",
            Self::QueueBounded => "bounded queue",
            Self::ShutdownDeadline => "shutdown deadline",
            Self::IndexRebuildable => "index rebuildability",
            Self::VerificationIntegrity => "verification integrity",
            Self::RepairLedgerCoverage => "repair ledger coverage",
            Self::RedactionNegativeTests => "redaction negative tests",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Assertion {
    pub kind: AssertionKind,
    pub status: AssertionStatus,
    pub detail: String,
}

pub async fn evaluate(
    incident_dir: &Path,
    watcher: Option<&Snapshot>,
    journal_pending: Option<usize>,
    incident_count: Option<u64>,
    index_count: Option<u64>,
    ledger: Option<&[LedgerEntry]>,
) -> Vec<Assertion> {
    let mut assertions = vec![
        observation_durability(watcher, journal_pending),
        source_isolation(watcher),
        queue_bounded(watcher),
        shutdown_deadline(watcher),
        index_rebuildable(incident_count, index_count),
        verification_integrity(ledger),
        redaction_negative_tests(),
    ];
    assertions.push(repair_ledger_coverage(incident_dir, ledger).await);
    assertions
}

fn supports_runtime_contract(snapshot: &Snapshot) -> bool {
    snapshot.runtime_contract_version >= RUNTIME_CONTRACT_VERSION
}

fn observation_durability(
    snapshot: Option<&Snapshot>,
    journal_pending: Option<usize>,
) -> Assertion {
    let Some(snapshot) = snapshot.filter(|snapshot| supports_runtime_contract(snapshot)) else {
        return unknown(
            AssertionKind::ObservationDurability,
            "runtime has not published accepted-observation evidence yet",
        );
    };
    let Some(journal_pending) = journal_pending else {
        return fail(
            AssertionKind::ObservationDurability,
            "durable journal could not be validated",
        );
    };
    let settled = snapshot
        .persisted
        .saturating_add(snapshot.grouped)
        .saturating_add(snapshot.deduplicated);
    let protected = settled.saturating_add(journal_pending as u64);
    if snapshot.accepted <= protected {
        pass(
            AssertionKind::ObservationDurability,
            format!(
                "accepted={} settled={} durable_journal={journal_pending}",
                snapshot.accepted, settled
            ),
        )
    } else {
        fail(
            AssertionKind::ObservationDurability,
            format!(
                "{} accepted observation(s) lack a settled outcome or durable journal entry",
                snapshot.accepted - protected
            ),
        )
    }
}

fn source_isolation(snapshot: Option<&Snapshot>) -> Assertion {
    let Some(snapshot) = snapshot.filter(|snapshot| supports_runtime_contract(snapshot)) else {
        return unknown(
            AssertionKind::SourceIsolation,
            "runtime has not published source-worker isolation evidence yet",
        );
    };
    if snapshot.source_workers_isolated {
        pass(
            AssertionKind::SourceIsolation,
            format!(
                "{} source worker(s) run independently",
                snapshot.sources.len()
            ),
        )
    } else {
        fail(
            AssertionKind::SourceIsolation,
            "runtime did not attest independent source workers",
        )
    }
}

fn queue_bounded(snapshot: Option<&Snapshot>) -> Assertion {
    let Some(snapshot) = snapshot else {
        return unknown(
            AssertionKind::QueueBounded,
            "watcher snapshot is unavailable",
        );
    };
    if snapshot.queue_capacity > 0
        && snapshot.queue_depth <= snapshot.queue_capacity
        && snapshot.queue_overflow_count == 0
    {
        pass(
            AssertionKind::QueueBounded,
            format!(
                "depth={}/{} overflow_count=0",
                snapshot.queue_depth, snapshot.queue_capacity
            ),
        )
    } else {
        fail(
            AssertionKind::QueueBounded,
            format!(
                "depth={}/{} overflow_count={}",
                snapshot.queue_depth, snapshot.queue_capacity, snapshot.queue_overflow_count
            ),
        )
    }
}

fn shutdown_deadline(snapshot: Option<&Snapshot>) -> Assertion {
    let Some(snapshot) = snapshot.filter(|snapshot| supports_runtime_contract(snapshot)) else {
        return unknown(
            AssertionKind::ShutdownDeadline,
            "runtime has not published shutdown deadline evidence yet",
        );
    };
    if snapshot.shutdown_deadline_ms == 0 {
        return fail(
            AssertionKind::ShutdownDeadline,
            "shutdown deadline is not bounded",
        );
    }
    if let Some(duration) = snapshot.last_shutdown_duration_ms
        && duration > snapshot.shutdown_deadline_ms
    {
        return fail(
            AssertionKind::ShutdownDeadline,
            format!(
                "last={duration}ms deadline={}ms",
                snapshot.shutdown_deadline_ms
            ),
        );
    }
    pass(
        AssertionKind::ShutdownDeadline,
        snapshot.last_shutdown_duration_ms.map_or_else(
            || {
                format!(
                    "enforced deadline={}ms; no completed shutdown yet",
                    snapshot.shutdown_deadline_ms
                )
            },
            |duration| {
                format!(
                    "last={duration}ms deadline={}ms",
                    snapshot.shutdown_deadline_ms
                )
            },
        ),
    )
}

fn index_rebuildable(incident_count: Option<u64>, index_count: Option<u64>) -> Assertion {
    match (incident_count, index_count) {
        (Some(json), Some(index)) if json == index => pass(
            AssertionKind::IndexRebuildable,
            format!("JSON={json} projection={index}"),
        ),
        (Some(json), Some(index)) => fail(
            AssertionKind::IndexRebuildable,
            format!("JSON={json} projection={index}"),
        ),
        _ => fail(
            AssertionKind::IndexRebuildable,
            "JSON source or rebuilt projection could not be validated",
        ),
    }
}

fn verification_integrity(ledger: Option<&[LedgerEntry]>) -> Assertion {
    let Some(ledger) = ledger else {
        return fail(
            AssertionKind::VerificationIntegrity,
            "ledger integrity validation failed",
        );
    };
    let contradictory_entry = ledger.iter().any(|entry| {
        entry.status == IncidentStatus::VerifiedFixed
            && entry
                .verifier
                .as_ref()
                .and_then(|value| value.get("passed"))
                .and_then(Value::as_bool)
                == Some(false)
    });
    let failed = ledger
        .iter()
        .filter_map(|entry| entry.timeline.as_ref())
        .filter(|event| {
            event.transition == TimelineTransition::Verified
                && event.outcome == TimelineOutcome::Failed
        })
        .filter_map(|event| event.repair_transaction_id.map(|id| id.to_string()))
        .collect::<BTreeSet<_>>();
    let committed = ledger
        .iter()
        .filter_map(|entry| entry.timeline.as_ref())
        .filter(|event| event.transition == TimelineTransition::Committed)
        .filter_map(|event| event.repair_transaction_id.map(|id| id.to_string()))
        .collect::<BTreeSet<_>>();
    if contradictory_entry || !failed.is_disjoint(&committed) {
        fail(
            AssertionKind::VerificationIntegrity,
            "a failed verification is represented as a successful outcome",
        )
    } else {
        pass(
            AssertionKind::VerificationIntegrity,
            format!("{} ledger entry/entries audited", ledger.len()),
        )
    }
}

async fn repair_ledger_coverage(incident_dir: &Path, ledger: Option<&[LedgerEntry]>) -> Assertion {
    let Some(ledger) = ledger else {
        return fail(
            AssertionKind::RepairLedgerCoverage,
            "ledger integrity validation failed",
        );
    };
    let outcomes = match repair_outcome_ids(incident_dir).await {
        Ok(ids) => ids,
        Err(detail) => return fail(AssertionKind::RepairLedgerCoverage, detail),
    };
    let ledger_ids = ledger
        .iter()
        .flat_map(ledger_repair_ids)
        .collect::<BTreeSet<_>>();
    let missing = outcomes.difference(&ledger_ids).count();
    if missing == 0 {
        pass(
            AssertionKind::RepairLedgerCoverage,
            format!("{} durable outcome(s) linked", outcomes.len()),
        )
    } else {
        fail(
            AssertionKind::RepairLedgerCoverage,
            format!("{missing} durable repair outcome(s) lack a ledger link"),
        )
    }
}

async fn repair_outcome_ids(incident_dir: &Path) -> Result<BTreeSet<String>, String> {
    let root = incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("transactions");
    let Ok(mut directories) = tokio::fs::read_dir(&root).await else {
        return Ok(BTreeSet::new());
    };
    let mut ids = BTreeSet::new();
    let mut scanned = 0_usize;
    while let Some(directory) = directories
        .next_entry()
        .await
        .map_err(|_| "transaction directory could not be scanned".to_string())?
    {
        for name in ["transaction.json", "operational-receipt.json"] {
            let path = directory.path().join(name);
            let Ok(metadata) = tokio::fs::metadata(&path).await else {
                continue;
            };
            scanned += 1;
            if scanned > MAX_REPAIR_RECEIPTS || metadata.len() > MAX_RECEIPT_BYTES {
                return Err("repair receipt audit exceeded its local bound".into());
            }
            let value: Value = serde_json::from_slice(
                &tokio::fs::read(&path)
                    .await
                    .map_err(|_| "repair receipt could not be read".to_string())?,
            )
            .map_err(|_| "repair receipt is invalid".to_string())?;
            let is_outcome = name == "operational-receipt.json"
                || matches!(
                    value.get("state").and_then(Value::as_str),
                    Some("verified" | "rolled_back")
                );
            if is_outcome {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "repair receipt has no typed identifier".to_string())?;
                ids.insert(id.to_string());
            }
        }
    }
    Ok(ids)
}

fn ledger_repair_ids(entry: &LedgerEntry) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = entry
        .timeline
        .as_ref()
        .and_then(|timeline| timeline.repair_transaction_id)
    {
        ids.push(id.to_string());
    }
    for value in [&entry.repair, &entry.after_state] {
        if let Some(id) = value
            .as_ref()
            .and_then(|value| {
                value
                    .get("repair_transaction_id")
                    .or_else(|| value.get("id"))
            })
            .and_then(Value::as_str)
        {
            ids.push(id.to_string());
        }
    }
    ids
}

fn redaction_negative_tests() -> Assertion {
    let (log_passed, log_total) = crate::logging::redaction_probe();
    let (analysis_passed, analysis_total) = analysis_redaction_probe();
    let passed = log_passed + analysis_passed;
    let total = log_total + analysis_total;
    if passed == total {
        pass(
            AssertionKind::RedactionNegativeTests,
            format!("{passed}/{total} synthetic probes passed"),
        )
    } else {
        fail(
            AssertionKind::RedactionNegativeTests,
            format!("{passed}/{total} synthetic probes passed"),
        )
    }
}

fn analysis_redaction_probe() -> (usize, usize) {
    let sentinels = ["probe-secret-argument", "/private/probe/evidence"];
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("private_path".into(), Value::String(sentinels[1].into()));
    let mut incident = Incident::detected(
        "probe",
        IncidentKind::Crash,
        "probe",
        Evidence {
            source: "probe".into(),
            summary: "probe".into(),
            artifact: Some(sentinels[1].into()),
            fields,
        },
    );
    incident.launch_context = Some(rescueloop_core::LaunchContext {
        executable: "/private/probe/bin".into(),
        arguments: Some(vec![sentinels[0].into()]),
        working_directory: Some("/private/probe".into()),
    });
    let encoded =
        serde_json::to_string(&AnalysisRequest::bounded(incident, Vec::new())).unwrap_or_default();
    let passed = sentinels
        .iter()
        .filter(|sentinel| !encoded.contains(**sentinel))
        .count();
    (passed, sentinels.len())
}

fn pass(kind: AssertionKind, detail: impl Into<String>) -> Assertion {
    Assertion {
        kind,
        status: AssertionStatus::Pass,
        detail: detail.into(),
    }
}

fn fail(kind: AssertionKind, detail: impl Into<String>) -> Assertion {
    Assertion {
        kind,
        status: AssertionStatus::Fail,
        detail: detail.into(),
    }
}

fn unknown(kind: AssertionKind, detail: impl Into<String>) -> Assertion {
    Assertion {
        kind,
        status: AssertionStatus::Unknown,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rescueloop_ledger::CausalRelation;
    use uuid::Uuid;

    fn snapshot() -> Snapshot {
        Snapshot {
            schema_version: crate::watch_health::WATCH_HEALTH_SCHEMA_VERSION,
            version: "fixture".into(),
            pid: 1,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            shutdown_reason: None,
            last_shutdown_reason: None,
            sources: Vec::new(),
            runtime_contract_version: RUNTIME_CONTRACT_VERSION,
            source_workers_isolated: true,
            accepted: 3,
            received: 3,
            persisted: 1,
            grouped: 1,
            deduplicated: 0,
            queue_depth: 1,
            queue_capacity: 8,
            queue_overflow_count: 0,
            shutdown_deadline_ms: 30_000,
            last_shutdown_duration_ms: Some(20),
            log_write_errors: 0,
            log_export_drops: 0,
            metrics: crate::metrics::MetricsSnapshot::default(),
        }
    }

    fn ledger_entry(status: IncidentStatus, verifier: Option<Value>) -> LedgerEntry {
        LedgerEntry {
            schema_version: 1,
            id: Uuid::new_v4(),
            recorded_at: Utc::now(),
            incident_id: Uuid::new_v4(),
            application_name: None,
            application_fingerprint: "app".into(),
            environment_fingerprint: "env".into(),
            incident_fingerprint: "incident".into(),
            repair: None,
            before_state: None,
            after_state: None,
            verifier,
            status,
            relation: CausalRelation::LifecycleUpdate,
            related_entry: None,
            timeline: None,
            previous_hash: None,
            entry_hash: "fixture".into(),
        }
    }

    #[test]
    fn durability_requires_every_accepted_observation_to_be_protected() {
        let snapshot = snapshot();
        assert_eq!(
            observation_durability(Some(&snapshot), Some(1)).status,
            AssertionStatus::Pass
        );
        assert_eq!(
            observation_durability(Some(&snapshot), Some(0)).status,
            AssertionStatus::Fail
        );
        let mut legacy = snapshot;
        legacy.runtime_contract_version = 0;
        assert_eq!(
            observation_durability(Some(&legacy), Some(1)).status,
            AssertionStatus::Unknown
        );
    }

    #[test]
    fn queue_shutdown_and_redaction_assertions_are_measurable() {
        let mut snapshot = snapshot();
        assert_eq!(queue_bounded(Some(&snapshot)).status, AssertionStatus::Pass);
        snapshot.queue_depth = 9;
        assert_eq!(queue_bounded(Some(&snapshot)).status, AssertionStatus::Fail);
        snapshot.queue_depth = 0;
        snapshot.last_shutdown_duration_ms = Some(30_001);
        assert_eq!(
            shutdown_deadline(Some(&snapshot)).status,
            AssertionStatus::Fail
        );
        assert_eq!(redaction_negative_tests().status, AssertionStatus::Pass);
    }

    #[test]
    fn verification_failure_cannot_be_reported_as_fixed() {
        let contradictory = ledger_entry(
            IncidentStatus::VerifiedFixed,
            Some(serde_json::json!({"passed": false})),
        );
        assert_eq!(
            verification_integrity(Some(&[contradictory])).status,
            AssertionStatus::Fail
        );
        let honest = ledger_entry(
            IncidentStatus::RolledBack,
            Some(serde_json::json!({"passed": false})),
        );
        assert_eq!(
            verification_integrity(Some(&[honest])).status,
            AssertionStatus::Pass
        );
    }

    #[tokio::test]
    async fn durable_repair_outcome_requires_a_ledger_identifier() {
        let root = std::env::temp_dir().join(format!("rescueloop-slo-{}", Uuid::new_v4()));
        let incidents = root.join("incidents");
        let transaction_id = Uuid::new_v4().to_string();
        let transaction_dir = root.join("transactions").join(&transaction_id);
        tokio::fs::create_dir_all(&transaction_dir).await.unwrap();
        tokio::fs::write(
            transaction_dir.join("transaction.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": transaction_id,
                "state": "verified"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let missing = repair_ledger_coverage(&incidents, Some(&[])).await;
        assert_eq!(missing.status, AssertionStatus::Fail);

        let mut linked = ledger_entry(IncidentStatus::VerifiedFixed, None);
        linked.repair = Some(serde_json::json!({
            "repair_transaction_id": transaction_id
        }));
        let covered = repair_ledger_coverage(&incidents, Some(&[linked])).await;
        assert_eq!(covered.status, AssertionStatus::Pass);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
