use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use rescueloop_core::{
    AnalysisId, Incident, IncidentId, IncidentStatus, ObservationId, OccurrenceId, PlanId,
    RepairTransactionId, VerificationId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelation {
    InitialFailure,
    LifecycleUpdate,
    Regression,
    IncompleteRepair,
    NewFailure,
    VerificationStale,
    AdverseEffect,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimelineComponent {
    Detector,
    Normalizer,
    IncidentStore,
    Grouper,
    Analyzer,
    Planner,
    Approval,
    Repair,
    Verifier,
    Ledger,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimelineTransition {
    Observed,
    Normalized,
    Persisted,
    Grouped,
    Analyzed,
    PlanProposed,
    Approved,
    Applied,
    Verified,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimelineOutcome {
    Completed,
    Delayed,
    Refused,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTimelineEvent {
    pub schema_version: u16,
    pub occurred_at: DateTime<Utc>,
    pub correlation_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_id: Option<ObservationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident_id: Option<IncidentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence_id: Option<OccurrenceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_id: Option<AnalysisId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<PlanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_transaction_id: Option<RepairTransactionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_id: Option<VerificationId>,
    pub component: TimelineComponent,
    pub transition: TimelineTransition,
    pub outcome: TimelineOutcome,
    explanation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delay_or_refusal_reason: Option<String>,
}

impl NewTimelineEvent {
    pub fn bounded(
        correlation_id: Uuid,
        occurred_at: DateTime<Utc>,
        component: TimelineComponent,
        transition: TimelineTransition,
        outcome: TimelineOutcome,
        explanation: impl Into<String>,
        delay_or_refusal_reason: Option<String>,
    ) -> Result<Self> {
        let explanation = explanation.into();
        if explanation.is_empty() || explanation.len() > 240 {
            bail!("timeline explanation must contain 1..=240 bytes")
        }
        if delay_or_refusal_reason
            .as_ref()
            .is_some_and(|reason| reason.is_empty() || reason.len() > 160)
        {
            bail!("timeline delay or refusal reason must contain 1..=160 bytes")
        }
        Ok(Self {
            schema_version: 1,
            occurred_at,
            correlation_id,
            observation_id: None,
            incident_id: None,
            occurrence_id: None,
            analysis_id: None,
            plan_id: None,
            repair_transaction_id: None,
            verification_id: None,
            component,
            transition,
            outcome,
            explanation,
            delay_or_refusal_reason,
        })
    }

    pub fn with_incident_ids(mut self, incident: &Incident) -> Self {
        self.observation_id = Some(incident.observation_id());
        self.incident_id = Some(incident.incident_id());
        self.occurrence_id = Some(incident.occurrence_id());
        self
    }

    pub fn explanation(&self) -> &str {
        &self.explanation
    }

    pub fn delay_or_refusal_reason(&self) -> Option<&str> {
        self.delay_or_refusal_reason.as_deref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewLedgerEntry {
    pub incident: Incident,
    pub repair: Option<Value>,
    pub before_state: Option<Value>,
    pub after_state: Option<Value>,
    pub verifier: Option<Value>,
    pub status: IncidentStatus,
    /// Only `AdverseEffect` requires an explicit causal assertion. Other values
    /// are derived from stable fingerprints and prior entries.
    pub relation_override: Option<CausalRelation>,
    pub timeline: Option<NewTimelineEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub schema_version: u16,
    pub id: Uuid,
    pub recorded_at: DateTime<Utc>,
    pub incident_id: Uuid,
    pub application_name: Option<String>,
    pub application_fingerprint: String,
    pub environment_fingerprint: String,
    pub incident_fingerprint: String,
    pub repair: Option<Value>,
    pub before_state: Option<Value>,
    pub after_state: Option<Value>,
    pub verifier: Option<Value>,
    pub status: IncidentStatus,
    pub relation: CausalRelation,
    pub related_entry: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<NewTimelineEvent>,
    pub previous_hash: Option<String>,
    pub entry_hash: String,
}

#[tracing::instrument(name = "ledger.load", skip_all, err)]
pub async fn load(path: &Path) -> Result<Vec<LedgerEntry>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || load_locked(&path)).await?
}

fn parse_entries(content: &str) -> Result<Vec<LedgerEntry>> {
    let mut entries = Vec::new();
    let mut previous: Option<String> = None;
    for (index, line) in content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        let entry: LedgerEntry = serde_json::from_str(line)
            .with_context(|| format!("invalid ledger line {}", index + 1))?;
        if entry.previous_hash != previous {
            bail!("ledger hash-chain break at line {}", index + 1)
        }
        if calculate_hash(&entry)? != entry.entry_hash {
            bail!("ledger content tampering at line {}", index + 1)
        }
        previous = Some(entry.entry_hash.clone());
        entries.push(entry);
    }
    Ok(entries)
}

#[tracing::instrument(
    name = "ledger.append",
    skip(path, new),
    fields(incident_id = %new.incident.id, status = ?new.status),
    err
)]
pub async fn append(path: &Path, new: NewLedgerEntry) -> Result<LedgerEntry> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || append_locked(&path, new, AppendMode::Always))
        .await??
        .context("unconditional ledger append was skipped")
}

pub async fn append_if_missing(path: &Path, new: NewLedgerEntry) -> Result<Option<LedgerEntry>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || append_locked(&path, new, AppendMode::Incident)).await?
}

pub async fn append_timeline_if_missing(
    path: &Path,
    new: NewLedgerEntry,
) -> Result<Option<LedgerEntry>> {
    if new.timeline.is_none() {
        bail!("timeline append requires timeline metadata")
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || append_locked(&path, new, AppendMode::Timeline)).await?
}

#[derive(Clone, Copy)]
enum AppendMode {
    Always,
    Incident,
    Timeline,
}

fn load_locked(path: &Path) -> Result<Vec<LedgerEntry>> {
    let Some(file) = open_existing(path)? else {
        return Ok(Vec::new());
    };
    file.lock_shared()?;
    let result = read_entries(&file);
    FileExt::unlock(&file)?;
    result
}

fn open_existing(path: &Path) -> Result<Option<File>> {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_entries(file: &File) -> Result<Vec<LedgerEntry>> {
    let mut content = String::new();
    BufReader::new(file.try_clone()?).read_to_string(&mut content)?;
    parse_entries(&content)
}

fn append_locked(
    path: &Path,
    new: NewLedgerEntry,
    mode: AppendMode,
) -> Result<Option<LedgerEntry>> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let existed = path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    let prior = read_entries_for_append(&file, path)?;
    let duplicate = match mode {
        AppendMode::Always => false,
        AppendMode::Incident => prior
            .iter()
            .any(|entry| entry.incident_id == new.incident.id),
        AppendMode::Timeline => new.timeline.as_ref().is_some_and(|timeline| {
            prior.iter().any(|entry| {
                entry.incident_id == new.incident.id
                    && entry.timeline.as_ref().is_some_and(|existing| {
                        existing.correlation_id == timeline.correlation_id
                            && existing.transition == timeline.transition
                            && existing.outcome == timeline.outcome
                    })
            })
        }),
    };
    if duplicate {
        FileExt::unlock(&file)?;
        return Ok(None);
    }
    let (relation, related_entry) = classify(&prior, &new);
    let mut entry = LedgerEntry {
        schema_version: 1,
        id: Uuid::new_v4(),
        recorded_at: Utc::now(),
        incident_id: new.incident.id,
        application_name: new
            .incident
            .application_identity
            .as_ref()
            .map(|x| x.name.clone())
            .or(new.incident.application.clone()),
        application_fingerprint: new.incident.application_fingerprint(),
        environment_fingerprint: new.incident.environment_fingerprint(),
        incident_fingerprint: new.incident.fingerprint(),
        repair: new.repair,
        before_state: new.before_state,
        after_state: new.after_state,
        verifier: new.verifier,
        status: new.status,
        relation,
        related_entry,
        timeline: new.timeline,
        previous_hash: prior.last().map(|x| x.entry_hash.clone()),
        entry_hash: String::new(),
    };
    entry.entry_hash = calculate_hash(&entry)?;
    let mut encoded = serde_json::to_vec(&entry)?;
    encoded.push(b'\n');
    file.seek(SeekFrom::End(0))?;
    file.write_all(&encoded)?;
    file.sync_data()?;
    FileExt::unlock(&file)?;
    if !existed {
        sync_directory(parent)?;
    }
    Ok(Some(entry))
}

fn read_entries_for_append(file: &File, path: &Path) -> Result<Vec<LedgerEntry>> {
    let mut content = Vec::new();
    BufReader::new(file.try_clone()?).read_to_end(&mut content)?;
    if content.is_empty() || content.ends_with(b"\n") {
        return parse_entries(std::str::from_utf8(&content).context("ledger is not UTF-8")?);
    }
    let valid_length = content
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let valid = std::str::from_utf8(&content[..valid_length]).context("ledger is not UTF-8")?;
    let entries = parse_entries(valid)?;
    quarantine_torn_tail(path, &content[valid_length..])?;
    file.set_len(valid_length as u64)?;
    file.sync_data()?;
    tracing::error!(
        event = "ledger.torn_tail_recovered",
        quarantined_bytes = content.len() - valid_length,
        "Incomplete final ledger record was quarantined"
    );
    Ok(entries)
}

fn quarantine_torn_tail(path: &Path, tail: &[u8]) -> Result<()> {
    let destination = path.with_extension(format!("torn-{}.json", Uuid::new_v4()));
    let mut quarantine = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    quarantine.write_all(tail)?;
    quarantine.sync_all()?;
    sync_directory(path.parent().unwrap_or(Path::new(".")))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn classify(prior: &[LedgerEntry], new: &NewLedgerEntry) -> (CausalRelation, Option<Uuid>) {
    if new.relation_override == Some(CausalRelation::AdverseEffect) {
        return (CausalRelation::AdverseEffect, prior.last().map(|x| x.id));
    }
    let app_name = new
        .incident
        .application_identity
        .as_ref()
        .map(|x| x.name.as_str())
        .or(new.incident.application.as_deref());
    let Some(previous) = prior
        .iter()
        .rev()
        .find(|entry| entry.application_name.as_deref() == app_name)
    else {
        return (CausalRelation::InitialFailure, None);
    };
    if previous.incident_id == new.incident.id {
        return (CausalRelation::LifecycleUpdate, Some(previous.id));
    }
    let app_fp = new.incident.application_fingerprint();
    let env_fp = new.incident.environment_fingerprint();
    if previous.application_fingerprint != app_fp || previous.environment_fingerprint != env_fp {
        return (CausalRelation::VerificationStale, Some(previous.id));
    }
    if previous.incident_fingerprint != new.incident.fingerprint() {
        return (CausalRelation::NewFailure, Some(previous.id));
    }
    let relation = match previous.status {
        IncidentStatus::VerifiedFixed => CausalRelation::Regression,
        IncidentStatus::RepairApplied | IncidentStatus::VerificationPending => {
            CausalRelation::IncompleteRepair
        }
        _ => CausalRelation::Regression,
    };
    (relation, Some(previous.id))
}

fn calculate_hash(entry: &LedgerEntry) -> Result<String> {
    #[derive(Serialize)]
    struct Hashable<'a> {
        schema_version: u16,
        id: Uuid,
        recorded_at: DateTime<Utc>,
        incident_id: Uuid,
        application_name: &'a Option<String>,
        application_fingerprint: &'a str,
        environment_fingerprint: &'a str,
        incident_fingerprint: &'a str,
        repair: &'a Option<Value>,
        before_state: &'a Option<Value>,
        after_state: &'a Option<Value>,
        verifier: &'a Option<Value>,
        status: &'a IncidentStatus,
        relation: &'a CausalRelation,
        related_entry: &'a Option<Uuid>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeline: &'a Option<NewTimelineEvent>,
        previous_hash: &'a Option<String>,
    }
    let value = Hashable {
        schema_version: entry.schema_version,
        id: entry.id,
        recorded_at: entry.recorded_at,
        incident_id: entry.incident_id,
        application_name: &entry.application_name,
        application_fingerprint: &entry.application_fingerprint,
        environment_fingerprint: &entry.environment_fingerprint,
        incident_fingerprint: &entry.incident_fingerprint,
        repair: &entry.repair,
        before_state: &entry.before_state,
        after_state: &entry.after_state,
        verifier: &entry.verifier,
        status: &entry.status,
        relation: &entry.relation,
        related_entry: &entry.related_entry,
        timeline: &entry.timeline,
        previous_hash: &entry.previous_hash,
    };
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{ApplicationIdentity, Evidence, IncidentKind};
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn incident(code: &str, version: &str) -> Incident {
        let mut value = Incident::detected(
            "windows",
            IncidentKind::Crash,
            "failure",
            Evidence {
                source: "fixture".into(),
                summary: "fixture".into(),
                artifact: None,
                fields: BTreeMap::new(),
            },
        );
        value.application = Some("Demo".into());
        value.application_identity = Some(ApplicationIdentity {
            name: "Demo".into(),
            version: Some(version.into()),
            binary_sha256: Some(version.into()),
            ..Default::default()
        });
        value.normalized_failure.code = Some(code.into());
        value
    }

    #[test]
    fn timeline_metadata_rejects_unbounded_text() {
        assert!(
            NewTimelineEvent::bounded(
                Uuid::new_v4(),
                Utc::now(),
                TimelineComponent::Analyzer,
                TimelineTransition::Analyzed,
                TimelineOutcome::Failed,
                "x".repeat(241),
                None,
            )
            .is_err()
        );
        assert!(
            NewTimelineEvent::bounded(
                Uuid::new_v4(),
                Utc::now(),
                TimelineComponent::Approval,
                TimelineTransition::Approved,
                TimelineOutcome::Refused,
                "approval stopped",
                Some("x".repeat(161)),
            )
            .is_err()
        );
    }

    fn new(incident: Incident, status: IncidentStatus) -> NewLedgerEntry {
        NewLedgerEntry {
            incident,
            repair: None,
            before_state: None,
            after_state: None,
            verifier: None,
            status,
            relation_override: None,
            timeline: None,
        }
    }

    #[tokio::test]
    async fn classifies_regression_new_failure_and_stale_verification() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        let first = append(
            &path,
            new(incident("oom", "1"), IncidentStatus::VerifiedFixed),
        )
        .await
        .unwrap();
        assert_eq!(first.relation, CausalRelation::InitialFailure);
        let regression = append(&path, new(incident("oom", "1"), IncidentStatus::Detected))
            .await
            .unwrap();
        assert_eq!(regression.relation, CausalRelation::Regression);
        let other = append(
            &path,
            new(incident("access_violation", "1"), IncidentStatus::Detected),
        )
        .await
        .unwrap();
        assert_eq!(other.relation, CausalRelation::NewFailure);
        let updated = append(
            &path,
            new(incident("access_violation", "2"), IncidentStatus::Detected),
        )
        .await
        .unwrap();
        assert_eq!(updated.relation, CausalRelation::VerificationStale);
        assert_eq!(load(&path).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn detects_ledger_tampering() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        append(
            &path,
            new(incident("oom", "1"), IncidentStatus::VerifiedFixed),
        )
        .await
        .unwrap();
        let content = fs::read_to_string(&path)
            .unwrap()
            .replace("verified_fixed", "rolled_back");
        fs::write(&path, content).unwrap();
        assert!(load(&path).await.is_err());
    }

    #[tokio::test]
    async fn timeline_metadata_is_hash_protected() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        let mut entry = new(incident("oom", "1"), IncidentStatus::Investigating);
        entry.timeline = Some(
            NewTimelineEvent::bounded(
                Uuid::new_v4(),
                Utc::now(),
                TimelineComponent::Analyzer,
                TimelineTransition::Analyzed,
                TimelineOutcome::Completed,
                "bounded analysis completed",
                None,
            )
            .unwrap(),
        );
        append(&path, entry).await.unwrap();
        let content = fs::read_to_string(&path)
            .unwrap()
            .replace("bounded analysis completed", "bounded analysis tampered");
        fs::write(&path, content).unwrap();
        assert!(load(&path).await.is_err());
    }

    #[tokio::test]
    async fn entries_without_timeline_keep_the_legacy_json_shape() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        append(&path, new(incident("oom", "1"), IncidentStatus::Detected))
            .await
            .unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("\"timeline\""));
        assert_eq!(load(&path).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serializes_concurrent_process_style_appends() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        let tasks = (0..32)
            .map(|index| {
                let path = path.clone();
                tokio::spawn(async move {
                    append(
                        &path,
                        new(
                            incident(&format!("failure-{index}"), "1"),
                            IncidentStatus::Detected,
                        ),
                    )
                    .await
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        let entries = load(&path).await.unwrap();
        assert_eq!(entries.len(), 32);
        assert!(
            entries.windows(2).all(|pair| {
                pair[1].previous_hash.as_deref() == Some(pair[0].entry_hash.as_str())
            })
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn initial_entry_check_and_append_is_atomic() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        let incident = incident("oom", "1");
        let tasks = (0..16)
            .map(|_| {
                let path = path.clone();
                let incident = incident.clone();
                tokio::spawn(async move {
                    append_if_missing(&path, new(incident, IncidentStatus::Detected)).await
                })
            })
            .collect::<Vec<_>>();
        let mut appended = 0;
        for task in tasks {
            appended += usize::from(task.await.unwrap().unwrap().is_some());
        }
        assert_eq!(appended, 1);
        assert_eq!(load(&path).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn quarantines_a_torn_final_record_before_append() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("ledger.jsonl");
        append(&path, new(incident("oom", "1"), IncidentStatus::Detected))
            .await
            .unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"schema_version":1,"partial""#).unwrap();
        file.sync_all().unwrap();

        append(&path, new(incident("panic", "1"), IncidentStatus::Detected))
            .await
            .unwrap();
        assert_eq!(load(&path).await.unwrap().len(), 2);
        assert!(
            fs::read_dir(temp.path())
                .unwrap()
                .flatten()
                .any(|entry| { entry.file_name().to_string_lossy().contains(".torn-") })
        );
    }
}
