mod analysis;
mod identifiers;
mod incident;

pub use analysis::{
    AnalysisError, AnalysisProvider, AnalysisRequest, AnalysisResponse, EventSource,
    EvidenceAssessment, Hypothesis, IncidentCollector, ProposedAction,
};
pub use identifiers::*;
pub use incident::{
    ApplicationIdentity, Confidence, EnvironmentIdentity, Evidence, Incident, IncidentKind,
    IncidentStatus, LaunchContext, NormalizedFailure,
};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::{collections::BTreeMap, path::PathBuf};
    use uuid::Uuid;

    fn incident() -> Incident {
        let mut value = Incident::detected(
            "windows",
            IncidentKind::Crash,
            "crash",
            Evidence {
                source: "wer".into(),
                summary: "raw".into(),
                artifact: Some(PathBuf::from("C:/Users/alice/report.wer")),
                fields: BTreeMap::new(),
            },
        );
        value.application_identity = Some(ApplicationIdentity {
            name: "Demo".into(),
            version: Some("1.0".into()),
            binary_sha256: Some("abc".into()),
            architecture: Some("x86_64".into()),
            ..Default::default()
        });
        value.normalized_failure.code = Some("c0000005".into());
        value
    }

    #[test]
    fn fingerprint_ignores_unstable_and_private_fields() {
        let first = incident();
        let mut second = first.clone();
        second.id = Uuid::new_v4();
        second.observed_at = Utc::now();
        second.message = "different raw message".into();
        second.evidence[0].artifact = Some(PathBuf::from("C:/Users/bob/other.wer"));
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn fingerprint_changes_for_normalized_failure() {
        let first = incident();
        let mut second = first.clone();
        second.normalized_failure.faulting_module = Some("d3d9.dll".into());
        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn lifecycle_identifiers_are_stable_and_legacy_incidents_have_fallbacks() {
        let value = incident();
        assert_eq!(value.correlation_id(), value.observation_id().as_uuid());
        assert_eq!(value.incident_id().as_uuid(), value.id);
        assert_eq!(value.occurrence_id().as_uuid(), value.id);

        let mut json = serde_json::to_value(&value).unwrap();
        json.as_object_mut().unwrap().remove("observation_id");
        json.as_object_mut().unwrap().remove("occurrence_id");
        json.as_object_mut().unwrap().remove("correlation_id");
        let legacy: Incident = serde_json::from_value(json).unwrap();
        assert_eq!(legacy.observation_id().as_uuid(), legacy.id);
        assert_eq!(legacy.occurrence_id().as_uuid(), legacy.id);
    }

    #[test]
    fn analysis_packet_is_bounded_and_redacted_but_keeps_opaque_target_id() {
        let mut value = incident();
        value.evidence[0]
            .fields
            .insert("container_id".into(), serde_json::json!("opaque-123"));
        value.evidence[0]
            .fields
            .insert("private_home".into(), serde_json::json!("/Users/alice"));
        value.evidence[0].fields.insert(
            "diagnostic_output".into(),
            serde_json::json!(
                (0..40)
                    .map(|index| format!("error {index}"))
                    .collect::<Vec<_>>()
            ),
        );
        let request = AnalysisRequest::bounded(value, vec!["restart_container".into()]);
        assert_eq!(request.schema_version, 3);
        assert!(request.incident.evidence[0].artifact.is_none());
        assert!(
            !request.incident.evidence[0]
                .fields
                .contains_key("private_home")
        );
        assert_eq!(
            request.incident.evidence[0].fields["container_id"],
            "opaque-123"
        );
        assert_eq!(
            request.incident.evidence[0].fields["diagnostic_output"]
                .as_array()
                .unwrap()
                .len(),
            30
        );
        assert!(request.evidence_assessment.redacted_fields >= 2);
    }
}
