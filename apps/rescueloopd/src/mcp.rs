use futures::StreamExt;
use rescueloop_core::AnalysisRequest;
use rmcp::{
    ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{CallToolResult, ServerInfo},
    tool, tool_router,
    transport::async_rw::JsonRpcMessageCodec,
};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio_util::codec::{FramedRead, FramedWrite};
use uuid::Uuid;

use crate::{incident_store::incidents_read_only, storage};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct RescueLoopMcp {
    incident_dir: PathBuf,
    log_health: Option<crate::logging::LogHealth>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListIncidentsInput {
    /// Maximum number of newest incidents to return. Range: 1 through 100.
    #[schemars(range(min = 1, max = 100), transform = remove_format)]
    #[serde(default = "default_limit")]
    limit: u32,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Debug, Serialize, JsonSchema)]
struct ListIncidentsOutput {
    incidents: Vec<IncidentSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct IncidentSummary {
    id: String,
    observed_at: String,
    platform: String,
    kind: String,
    confidence: String,
    application: String,
    status: String,
    #[schemars(transform = remove_format)]
    occurrence_count: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetIncidentInput {
    /// UUID returned by list_incidents. Filesystem paths are not accepted.
    incident_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetIncidentOutput {
    #[schemars(transform = remove_format)]
    schema_version: u16,
    /// Bounded incident JSON after RescueLoop privacy redaction.
    incident: IncidentDetail,
    /// Completeness and redaction metadata for the returned evidence.
    evidence_assessment: EvidenceAssessment,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetIncidentTimelineOutput {
    #[schemars(transform = remove_format)]
    schema_version: u16,
    events: Vec<TimelineEventOutput>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TimelineEventOutput {
    timestamp: String,
    correlation_id: String,
    observation_id: String,
    incident_id: String,
    occurrence_id: String,
    analysis_id: String,
    plan_id: String,
    repair_transaction_id: String,
    verification_id: String,
    component: String,
    lifecycle_transition: String,
    outcome: String,
    explanation: String,
    ledger_entry_id: String,
    delay_or_refusal_reason: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct IncidentDetail {
    id: String,
    observation_id: String,
    occurrence_id: String,
    observed_at: String,
    platform: String,
    kind: String,
    confidence: String,
    application: String,
    message: String,
    status: String,
    #[schemars(transform = remove_format)]
    occurrence_count: u64,
    first_observed_at: String,
    last_observed_at: String,
    launch_executable: String,
    application_identity: BTreeMap<String, String>,
    environment_identity: BTreeMap<String, String>,
    normalized_failure: BTreeMap<String, String>,
    evidence: Vec<RedactedEvidence>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RedactedEvidence {
    source: String,
    summary: String,
    /// Allowlisted values encoded as compact JSON strings to keep the MCP schema portable.
    fields: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct EvidenceAssessment {
    #[schemars(transform = remove_format)]
    completeness: f32,
    missing: Vec<String>,
    #[schemars(transform = remove_format)]
    redacted_fields: u32,
    #[schemars(transform = remove_format)]
    retained_evidence: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetAgentHealthOutput {
    #[schemars(transform = remove_format)]
    schema_version: u16,
    version: String,
    platform: PlatformOutput,
    overall_status: String,
    watcher_uptime: UptimeOutput,
    last_shutdown: LastShutdownOutput,
    components: Vec<HealthCheckOutput>,
    slo_assertions: Vec<SloAssertionOutput>,
    pipeline: PipelineOutput,
}

#[derive(Debug, Serialize, JsonSchema)]
struct UptimeOutput {
    available: bool,
    #[schemars(transform = remove_format)]
    seconds: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LastShutdownOutput {
    available: bool,
    reason: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PlatformOutput {
    os: String,
    architecture: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct HealthCheckOutput {
    name: String,
    status: String,
    detail: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SloAssertionOutput {
    assertion: String,
    status: String,
    detail: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PipelineOutput {
    #[schemars(transform = remove_format)]
    received: u64,
    #[schemars(transform = remove_format)]
    persisted: u64,
    #[schemars(transform = remove_format)]
    grouped: u64,
    #[schemars(transform = remove_format)]
    deduplicated: u64,
    #[schemars(transform = remove_format)]
    queue_depth: usize,
    #[schemars(transform = remove_format)]
    queue_capacity: usize,
    #[schemars(transform = remove_format)]
    journal_pending: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListEventSourcesOutput {
    #[schemars(transform = remove_format)]
    schema_version: u16,
    sources: Vec<EventSourceOutput>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct EventSourceOutput {
    name: String,
    status: String,
    last_success: LastSuccessOutput,
    #[schemars(transform = remove_format)]
    received: u64,
    #[schemars(transform = remove_format)]
    dropped: u64,
    #[schemars(transform = remove_format)]
    deduplicated: u64,
    #[schemars(transform = remove_format)]
    reconnect_count: u64,
    #[schemars(transform = remove_format)]
    backoff_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LastSuccessOutput {
    available: bool,
    timestamp: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetLocalMetricsSummaryOutput {
    #[schemars(transform = remove_format)]
    schema_version: u16,
    #[schemars(transform = remove_formats_recursive)]
    events_received_total: BTreeMap<String, u64>,
    #[schemars(transform = remove_formats_recursive)]
    events_dropped_total: BTreeMap<String, u64>,
    #[schemars(transform = remove_format)]
    source_reconnects_total: u64,
    #[schemars(transform = remove_format)]
    queue_depth: u64,
    durations: BTreeMap<String, DurationSummaryOutput>,
    #[schemars(transform = remove_format)]
    rollback_total: u64,
    #[schemars(transform = remove_format)]
    log_write_failures_total: u64,
    #[schemars(transform = remove_format)]
    index_rebuild_total: u64,
    #[schemars(transform = remove_format)]
    journal_pending_count: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DurationSummaryOutput {
    #[schemars(transform = remove_format)]
    count: u64,
    #[schemars(transform = remove_format)]
    total_micros: u64,
    #[schemars(transform = remove_format)]
    max_micros: u64,
    #[schemars(transform = remove_format)]
    last_micros: u64,
}

fn default_limit() -> u32 {
    20
}

fn remove_format(schema: &mut Schema) {
    schema.remove("format");
}

fn remove_formats_recursive(schema: &mut Schema) {
    fn visit(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("format");
                for child in object.values_mut() {
                    visit(child);
                }
            }
            Value::Array(array) => {
                for child in array {
                    visit(child);
                }
            }
            _ => {}
        }
    }
    if let Some(object) = schema.as_object_mut() {
        object.remove("format");
        for value in object.values_mut() {
            visit(value);
        }
    }
}

pub async fn serve(
    incident_dir: &Path,
    log_health: crate::logging::LogHealth,
) -> anyhow::Result<()> {
    let incident_dir = storage::prepare_mcp_store(incident_dir)?;
    let reader = FramedRead::new(
        tokio::io::stdin(),
        JsonRpcMessageCodec::<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>::new_with_max_length(
            MAX_MESSAGE_BYTES,
        ),
    )
    .filter_map(|result| futures::future::ready(match result {
        Ok(message) => Some(message),
        Err(error) => {
            tracing::warn!(event = "mcp.input_rejected", %error, "Rejected MCP input");
            None
        }
    }));
    let writer = FramedWrite::new(
        tokio::io::stdout(),
        JsonRpcMessageCodec::<rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>>::default(),
    );
    let service = RescueLoopMcp {
        incident_dir,
        log_health: Some(log_health),
    }
    .serve((writer, reader))
    .await?;
    service.waiting().await?;
    Ok(())
}

#[tool_router]
impl RescueLoopMcp {
    #[tool(
        description = "List redacted summaries of locally stored RescueLoop incidents.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_incidents(
        &self,
        Parameters(input): Parameters<ListIncidentsInput>,
    ) -> Result<Json<ListIncidentsOutput>, CallToolResult> {
        let limit = input.limit;
        if !(1..=100).contains(&limit) {
            return Err(tool_error("limit must be between 1 and 100"));
        }
        let values = self.read_incidents().await?;
        let incidents = values
            .into_iter()
            .take(limit as usize)
            .map(|(incident, _)| IncidentSummary {
                id: incident.id.to_string(),
                observed_at: incident.observed_at.to_rfc3339(),
                platform: incident.platform,
                kind: camel_to_snake(&format!("{:?}", incident.kind)),
                confidence: format!("{:?}", incident.confidence).to_ascii_lowercase(),
                application: incident.application.unwrap_or_else(|| "unknown".into()),
                status: camel_to_snake(&format!("{:?}", incident.status)),
                occurrence_count: incident.occurrence_count,
            })
            .collect();
        Ok(Json(ListIncidentsOutput { incidents }))
    }

    #[tool(
        description = "Get one bounded and redacted incident by UUID. Local paths and launch arguments are removed.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_incident(
        &self,
        Parameters(input): Parameters<GetIncidentInput>,
    ) -> Result<Json<GetIncidentOutput>, CallToolResult> {
        let id = Uuid::parse_str(&input.incident_id)
            .map_err(|_| tool_error("incident_id must be a UUID"))?;
        let incident = self
            .read_incidents()
            .await?
            .into_iter()
            .find_map(|(incident, _)| (incident.id == id).then_some(incident))
            .ok_or_else(|| tool_error("incident not found"))?;
        let packet = AnalysisRequest::bounded(incident, Vec::new());
        Ok(Json(GetIncidentOutput {
            schema_version: packet.schema_version,
            incident: IncidentDetail::from(packet.incident),
            evidence_assessment: EvidenceAssessment {
                completeness: packet.evidence_assessment.completeness,
                missing: packet.evidence_assessment.missing,
                redacted_fields: packet.evidence_assessment.redacted_fields,
                retained_evidence: packet.evidence_assessment.retained_evidence,
            },
        }))
    }

    #[tool(
        description = "Get the bounded hash-linked lifecycle timeline for one incident by UUID.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_incident_timeline(
        &self,
        Parameters(input): Parameters<GetIncidentInput>,
    ) -> Result<Json<GetIncidentTimelineOutput>, CallToolResult> {
        let id = Uuid::parse_str(&input.incident_id)
            .map_err(|_| tool_error("incident_id must be a UUID"))?;
        let incident = self
            .read_incidents()
            .await?
            .into_iter()
            .find_map(|(incident, _)| (incident.id == id).then_some(incident))
            .ok_or_else(|| tool_error("incident not found"))?;
        let events = crate::timeline::load(&self.incident_dir, &incident)
            .await
            .map_err(|error| {
                tracing::error!(event = "mcp.timeline_read_failed", %error, "MCP timeline read failed");
                tool_error("the local timeline is unavailable or failed integrity checks")
            })?
            .into_iter()
            .map(|event| TimelineEventOutput {
                timestamp: event.timestamp.to_rfc3339(),
                correlation_id: event.correlation_id.to_string(),
                observation_id: event.observation_id.map_or_else(String::new, |id| id.to_string()),
                incident_id: event.incident_id.map_or_else(String::new, |id| id.to_string()),
                occurrence_id: event.occurrence_id.map_or_else(String::new, |id| id.to_string()),
                analysis_id: event.analysis_id.map_or_else(String::new, |id| id.to_string()),
                plan_id: event.plan_id.map_or_else(String::new, |id| id.to_string()),
                repair_transaction_id: event.repair_transaction_id.map_or_else(String::new, |id| id.to_string()),
                verification_id: event.verification_id.map_or_else(String::new, |id| id.to_string()),
                component: camel_to_snake(&format!("{:?}", event.component)),
                lifecycle_transition: camel_to_snake(&format!(
                    "{:?}",
                    event.lifecycle_transition
                )),
                outcome: camel_to_snake(&format!("{:?}", event.outcome)),
                explanation: event.explanation,
                ledger_entry_id: event.ledger_entry_id.to_string(),
                delay_or_refusal_reason: event.delay_or_refusal_reason.unwrap_or_default(),
            })
            .collect();
        Ok(Json(GetIncidentTimelineOutput {
            schema_version: 1,
            events,
        }))
    }

    #[tool(
        description = "Get bounded local RescueLoop component health and SLO assertions. This tool cannot change agent state.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_agent_health(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<Json<GetAgentHealthOutput>, CallToolResult> {
        let snapshot = self.health_snapshot().await;
        let overall_status = overall_health(&snapshot);
        let watcher_uptime = UptimeOutput {
            available: snapshot.watcher_uptime_seconds.is_some(),
            seconds: snapshot.watcher_uptime_seconds.unwrap_or_default(),
        };
        let last_shutdown = LastShutdownOutput {
            available: snapshot.last_shutdown_reason.is_some(),
            reason: snapshot.last_shutdown_reason.clone().unwrap_or_default(),
        };
        Ok(Json(GetAgentHealthOutput {
            schema_version: 1,
            version: snapshot.version,
            platform: PlatformOutput {
                os: std::env::consts::OS.into(),
                architecture: std::env::consts::ARCH.into(),
            },
            overall_status,
            watcher_uptime,
            last_shutdown,
            components: snapshot
                .checks
                .into_iter()
                .map(|check| HealthCheckOutput {
                    name: check.name,
                    status: health_state(check.state),
                    detail: check.detail,
                })
                .collect(),
            slo_assertions: snapshot
                .slo_assertions
                .into_iter()
                .map(|assertion| SloAssertionOutput {
                    assertion: assertion.kind.label().replace(' ', "_"),
                    status: assertion.status.label().to_ascii_lowercase(),
                    detail: assertion.detail,
                })
                .collect(),
            pipeline: PipelineOutput {
                received: snapshot.received,
                persisted: snapshot.persisted,
                grouped: snapshot.grouped,
                deduplicated: snapshot.deduplicated,
                queue_depth: snapshot.queue_depth,
                queue_capacity: snapshot.queue_capacity,
                journal_pending: snapshot.journal_pending,
            },
        }))
    }

    #[tool(
        description = "List bounded local event-source health and counters. This tool cannot enable, disable, or reconnect sources.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_event_sources(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<Json<ListEventSourcesOutput>, CallToolResult> {
        let snapshot = self.health_snapshot().await;
        Ok(Json(ListEventSourcesOutput {
            schema_version: 1,
            sources: snapshot
                .sources
                .into_iter()
                .map(|source| {
                    let last_success = LastSuccessOutput {
                        available: source.last_success_at.is_some(),
                        timestamp: source
                            .last_success_at
                            .map(|value| value.to_rfc3339())
                            .unwrap_or_default(),
                    };
                    EventSourceOutput {
                        name: source.name,
                        status: camel_to_snake(&format!("{:?}", source.state)),
                        last_success,
                        received: source.received,
                        dropped: source.dropped,
                        deduplicated: source.deduplicated,
                        reconnect_count: source.reconnect_count,
                        backoff_ms: source.backoff_ms,
                    }
                })
                .collect(),
        }))
    }

    #[tool(
        description = "Get a bounded summary of typed process-local RescueLoop metrics. No exporter is enabled by this tool.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_local_metrics_summary(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<Json<GetLocalMetricsSummaryOutput>, CallToolResult> {
        let metrics = self.health_snapshot().await.metrics;
        let durations = [
            (
                "incident_persist_duration",
                metrics.incident_persist_duration,
            ),
            (
                "incident_grouping_duration",
                metrics.incident_grouping_duration,
            ),
            ("analysis_duration", metrics.analysis_duration),
            ("repair_duration", metrics.repair_duration),
            ("verification_duration", metrics.verification_duration),
        ]
        .into_iter()
        .map(|(name, duration)| (name.into(), duration_summary(duration)))
        .collect();
        Ok(Json(GetLocalMetricsSummaryOutput {
            schema_version: 1,
            events_received_total: metrics
                .events_received_total
                .into_iter()
                .map(|(source, count)| (camel_to_snake(&format!("{source:?}")), count))
                .collect(),
            events_dropped_total: metrics
                .events_dropped_total
                .into_iter()
                .map(|(reason, count)| (camel_to_snake(&format!("{reason:?}")), count))
                .collect(),
            source_reconnects_total: metrics.source_reconnects_total,
            queue_depth: metrics.queue_depth,
            durations,
            rollback_total: metrics.rollback_total,
            log_write_failures_total: metrics.log_write_failures_total,
            index_rebuild_total: metrics.index_rebuild_total,
            journal_pending_count: metrics.journal_pending_count,
        }))
    }

    async fn health_snapshot(&self) -> crate::doctor::DoctorSnapshot {
        let (write_errors, export_drops) = self.log_health.as_ref().map_or((0, 0), |health| {
            (health.write_errors(), health.export_drops())
        });
        crate::doctor::collect_read_only(&self.incident_dir, write_errors, export_drops).await
    }

    async fn read_incidents(
        &self,
    ) -> Result<Vec<(rescueloop_core::Incident, PathBuf)>, CallToolResult> {
        incidents_read_only(&self.incident_dir).await.map_err(|error| {
            tracing::error!(event = "mcp.store_read_failed", %error, "MCP incident read failed");
            tool_error("the local incident store is unavailable or failed integrity checks")
        })
    }
}

impl From<rescueloop_core::Incident> for IncidentDetail {
    fn from(incident: rescueloop_core::Incident) -> Self {
        let observation_id = incident.observation_id().to_string();
        let occurrence_id = incident.occurrence_id().to_string();
        let launch_executable = incident
            .launch_context
            .as_ref()
            .map(|context| context.executable.to_string_lossy().into_owned())
            .unwrap_or_default();
        let application_identity = incident
            .application_identity
            .as_ref()
            .map(|identity| string_map(serde_json::to_value(identity).unwrap_or_default()))
            .unwrap_or_default();
        let environment_identity = incident
            .environment_identity
            .as_ref()
            .map(|identity| string_map(serde_json::to_value(identity).unwrap_or_default()))
            .unwrap_or_default();
        let normalized_failure =
            string_map(serde_json::to_value(&incident.normalized_failure).unwrap_or_default());
        Self {
            id: incident.id.to_string(),
            observation_id,
            occurrence_id,
            observed_at: incident.observed_at.to_rfc3339(),
            platform: incident.platform,
            kind: camel_to_snake(&format!("{:?}", incident.kind)),
            confidence: format!("{:?}", incident.confidence).to_ascii_lowercase(),
            application: incident.application.unwrap_or_else(|| "unknown".into()),
            message: incident.message,
            status: camel_to_snake(&format!("{:?}", incident.status)),
            occurrence_count: incident.occurrence_count,
            first_observed_at: incident
                .first_observed_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_default(),
            last_observed_at: incident
                .last_observed_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_default(),
            launch_executable,
            application_identity,
            environment_identity,
            normalized_failure,
            evidence: incident
                .evidence
                .into_iter()
                .map(|evidence| RedactedEvidence {
                    source: evidence.source,
                    summary: evidence.summary,
                    fields: evidence
                        .fields
                        .into_iter()
                        .map(|(key, value)| (key, compact_json(value)))
                        .collect(),
                })
                .collect(),
        }
    }
}

fn string_map(value: Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), compact_json(value.clone())))
        .collect()
}

fn compact_json(value: Value) -> String {
    match value {
        Value::String(value) => value,
        other => {
            serde_json::to_string(&other).expect("serializing a serde_json::Value cannot fail")
        }
    }
}

#[rmcp::tool_handler]
impl rmcp::ServerHandler for RescueLoopMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default()
            .with_server_info(rmcp::model::Implementation::new(
                "rescueloop",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Read-only access to bounded, redacted RescueLoop incidents, lifecycle timelines, agent health, event-source status, and local metrics summaries. No repair, replay, rollback, approval, arbitrary file, path, or shell tools are exposed.",
            );
        info.capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        info
    }
}

fn tool_error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::ContentBlock::text(message.to_owned())])
}

fn overall_health(snapshot: &crate::doctor::DoctorSnapshot) -> String {
    if snapshot.checks.iter().any(|check| {
        check.name == "watcher" && check.state == crate::doctor::HealthState::Disconnected
    }) {
        "disconnected".into()
    } else if snapshot
        .checks
        .iter()
        .any(|check| check.state != crate::doctor::HealthState::Healthy)
        || snapshot
            .slo_assertions
            .iter()
            .any(|assertion| assertion.status != crate::slo::AssertionStatus::Pass)
    {
        "degraded".into()
    } else {
        "healthy".into()
    }
}

fn health_state(state: crate::doctor::HealthState) -> String {
    camel_to_snake(&format!("{state:?}"))
}

fn duration_summary(duration: crate::metrics::DurationSnapshot) -> DurationSummaryOutput {
    DurationSummaryOutput {
        count: duration.count,
        total_micros: duration.total_micros,
        max_micros: duration.max_micros,
        last_micros: duration.last_micros,
    }
}

fn camel_to_snake(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        if character.is_ascii_uppercase() && index > 0 {
            result.push('_');
        }
        result.push(character.to_ascii_lowercase());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rescueloop_core::{Evidence, Incident, IncidentKind, LaunchContext};
    use rmcp::model::CallToolRequestParams;
    use std::collections::BTreeMap;
    use tokio::io::duplex;
    use tokio_util::codec::Decoder;

    async fn connected_server(
        incident_dir: PathBuf,
    ) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleClient, ()>> {
        let (client_transport, server_transport) = duplex(64 * 1024);
        tokio::spawn(async move {
            RescueLoopMcp {
                incident_dir,
                log_health: None,
            }
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
        });
        Ok(().serve(client_transport).await?)
    }

    #[test]
    fn exposes_only_read_only_tools() {
        let tools = RescueLoopMcp::tool_router().list_all();
        assert_eq!(tools.len(), 6);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "get_agent_health",
                "get_incident",
                "get_incident_timeline",
                "get_local_metrics_summary",
                "list_event_sources",
                "list_incidents",
            ])
        );
        assert!(tools.iter().all(|tool| tool.output_schema.is_some()));
        assert!(tools.iter().all(|tool| {
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true)
        }));
        assert!(!tools.iter().any(|tool| tool.name.contains("repair")));
        assert!(!tools.iter().any(|tool| {
            ["replay", "rollback", "approve", "apply", "write", "path"]
                .iter()
                .any(|forbidden| tool.name.contains(forbidden))
        }));
        assert!(tools.iter().all(|tool| {
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            !schema_property_names(&schema)
                .iter()
                .any(|name| name.to_ascii_lowercase().contains("path"))
        }));
        for expected in [
            "get_agent_health",
            "list_event_sources",
            "get_incident_timeline",
            "get_local_metrics_summary",
        ] {
            assert!(tools.iter().any(|tool| tool.name == expected));
        }
        let info = <RescueLoopMcp as rmcp::ServerHandler>::get_info(&RescueLoopMcp {
            incident_dir: PathBuf::from("unused"),
            log_health: None,
        });
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_none());
    }

    #[test]
    fn rejects_oversized_protocol_messages() {
        let mut codec = JsonRpcMessageCodec::<
            rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>,
        >::new_with_max_length(MAX_MESSAGE_BYTES);
        let mut input = bytes::BytesMut::from(vec![b'x'; MAX_MESSAGE_BYTES + 1].as_slice());
        input.extend_from_slice(b"\n");
        assert!(codec.decode(&mut input).is_err());
    }

    #[tokio::test]
    async fn validates_arguments_and_returns_tool_errors() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let client = connected_server(root.clone()).await.unwrap();
        let invalid = client
            .call_tool(CallToolRequestParams::new("list_incidents").with_arguments(
                serde_json::Map::from_iter([("limit".into(), Value::String("bad".into()))]),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.is_error, Some(true));
        let unknown = client
            .call_tool(CallToolRequestParams::new("list_incidents").with_arguments(
                serde_json::Map::from_iter([("unexpected".into(), Value::Bool(true))]),
            ))
            .await
            .unwrap();
        assert_eq!(unknown.is_error, Some(true));
        let missing = client
            .call_tool(CallToolRequestParams::new("get_incident").with_arguments(
                serde_json::Map::from_iter([(
                    "incident_id".into(),
                    Value::String(Uuid::new_v4().to_string()),
                )]),
            ))
            .await
            .unwrap();
        assert_eq!(missing.is_error, Some(true));
        for incident_id in ["not-a-uuid", "../outside"] {
            let invalid_timeline = client
                .call_tool(
                    CallToolRequestParams::new("get_incident_timeline").with_arguments(
                        serde_json::Map::from_iter([(
                            "incident_id".into(),
                            Value::String(incident_id.into()),
                        )]),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(invalid_timeline.is_error, Some(true));
        }
        let missing_timeline = client
            .call_tool(
                CallToolRequestParams::new("get_incident_timeline").with_arguments(
                    serde_json::Map::from_iter([(
                        "incident_id".into(),
                        Value::String(Uuid::new_v4().to_string()),
                    )]),
                ),
            )
            .await
            .unwrap();
        assert_eq!(missing_timeline.is_error, Some(true));
        for tool in [
            "get_agent_health",
            "list_event_sources",
            "get_local_metrics_summary",
        ] {
            let invalid = client
                .call_tool(CallToolRequestParams::new(tool).with_arguments(
                    serde_json::Map::from_iter([("unexpected".into(), Value::Bool(true))]),
                ))
                .await
                .unwrap();
            assert_eq!(invalid.is_error, Some(true));
        }
        client.cancel().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn returns_object_structured_content_and_redacts_private_data() {
        let state_root = test_root();
        let incident_dir = state_root.join("incidents");
        tokio::fs::create_dir_all(&incident_dir).await.unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("private_home".into(), serde_json::json!("/Users/alice"));
        fields.insert("exit_code".into(), serde_json::json!(1));
        let mut incident = Incident::detected(
            "test",
            IncidentKind::Crash,
            "failure",
            Evidence {
                source: "test".into(),
                summary: "test evidence".into(),
                artifact: Some(PathBuf::from("/Users/alice/private.crash")),
                fields,
            },
        );
        incident.launch_context = Some(LaunchContext {
            executable: PathBuf::from("/Users/alice/bin/private-app"),
            arguments: Some(vec!["--token=secret".into()]),
            working_directory: Some(PathBuf::from("/Users/alice/private")),
        });
        tokio::fs::write(
            incident_dir.join(format!("{}.json", incident.id)),
            serde_json::to_vec(&incident).unwrap(),
        )
        .await
        .unwrap();
        crate::timeline::ensure_initial(&incident_dir, &incident)
            .await
            .unwrap();
        let watch_health = crate::watch_health::WatchHealth::new(8);
        watch_health.source_started("fixture-source");
        watch_health.observation_received("fixture-source");
        watch_health.queued();
        watch_health.publish_to(&incident_dir, None).await.unwrap();
        let client = connected_server(incident_dir).await.unwrap();
        let listed = client
            .call_tool(CallToolRequestParams::new("list_incidents"))
            .await
            .unwrap();
        assert!(listed.structured_content.unwrap().is_object());
        let detail = client
            .call_tool(CallToolRequestParams::new("get_incident").with_arguments(
                serde_json::Map::from_iter([(
                    "incident_id".into(),
                    Value::String(incident.id.to_string()),
                )]),
            ))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert!(detail.is_object());
        assert_eq!(
            detail["incident"]["observation_id"],
            incident.observation_id().to_string()
        );
        assert_eq!(
            detail["incident"]["occurrence_id"],
            incident.occurrence_id().to_string()
        );
        assert!(detail["incident"]["evidence"][0].get("artifact").is_none());
        assert!(
            detail["incident"]["evidence"][0]["fields"]
                .get("private_home")
                .is_none()
        );
        assert_eq!(detail["incident"]["launch_executable"], "private-app");
        let serialized = serde_json::to_string(&detail).unwrap();
        assert!(!serialized.contains("--token=secret"));
        assert!(!serialized.contains("/Users/alice/private"));
        let timeline = client
            .call_tool(
                CallToolRequestParams::new("get_incident_timeline").with_arguments(
                    serde_json::Map::from_iter([(
                        "incident_id".into(),
                        Value::String(incident.id.to_string()),
                    )]),
                ),
            )
            .await
            .unwrap()
            .structured_content
            .unwrap();
        let timeline_serialized = serde_json::to_string(&timeline).unwrap();
        assert!(timeline_serialized.contains("ledger_entry_id"));
        assert!(timeline_serialized.contains(&incident.observation_id().to_string()));
        assert!(timeline_serialized.contains(&incident.incident_id().to_string()));
        assert!(timeline_serialized.contains(&incident.occurrence_id().to_string()));
        assert!(!timeline_serialized.contains("/Users/alice"));
        assert!(!timeline_serialized.contains("secret"));
        let health = client
            .call_tool(CallToolRequestParams::new("get_agent_health"))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(health["schema_version"], 1);
        assert!(health["components"].is_array());
        assert!(health["slo_assertions"].is_array());
        assert!(health["pipeline"].is_object());
        let sources = client
            .call_tool(CallToolRequestParams::new("list_event_sources"))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(sources["sources"][0]["name"], "fixture-source");
        assert_eq!(sources["sources"][0]["received"], 1);
        let metrics = client
            .call_tool(CallToolRequestParams::new("get_local_metrics_summary"))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(metrics["schema_version"], 1);
        assert!(metrics["durations"]["analysis_duration"].is_object());
        for output in [&health, &sources, &metrics] {
            let serialized = serde_json::to_string(output).unwrap();
            assert!(!serialized.contains("/Users/alice"));
            assert!(!serialized.contains("--token=secret"));
            assert!(!serialized.contains("private.crash"));
        }
        assert!(
            !state_root.join("index-v1.db").exists(),
            "read-only MCP calls must not create the disposable index"
        );
        client.cancel().await.unwrap();
        std::fs::remove_dir_all(state_root).unwrap();
    }

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("rescueloop-mcp-test-{}", Uuid::new_v4()))
    }

    fn schema_property_names(value: &Value) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(object) = value.as_object() {
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                names.extend(properties.keys().cloned());
            }
            for child in object.values() {
                names.extend(schema_property_names(child));
            }
        } else if let Some(array) = value.as_array() {
            for child in array {
                names.extend(schema_property_names(child));
            }
        }
        names
    }
}
