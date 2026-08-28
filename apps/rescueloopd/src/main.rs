use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rescueloop_agent::{ALLOWED_ACTIONS, HttpAnalysisProvider};
use rescueloop_core::{AnalysisProvider, AnalysisRequest, Incident};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::Instrument;
use tracing::{error, info};

mod console;
mod diagnostics;
mod doctor;
mod incident_store;
mod logging;
mod mcp;
mod metrics;
mod observation_journal;
mod repair_flow;
mod service;
mod slo;
mod storage;
mod timeline;
mod tui;
mod watch_health;
mod watcher;

pub(crate) use console::configured_provider;
use console::{console, index_command, setup, sources};
pub(crate) use incident_store::local_timestamp;
pub(crate) use incident_store::{dismiss_incident, record_incident_status};
use incident_store::{incidents, save_incident};
pub(crate) use repair_flow::{repair, repair_silent};

#[derive(Parser)]
#[command(
    name = "rescueloop",
    about = "Detect failures first; analyze only with explicit user intent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, default_value = ".rescueloop/incidents", global = true)]
    incident_dir: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Start the background watcher if needed, then open the console.
    Start,
    /// Stop the background watcher without removing its registration.
    Stop,
    /// Show whether the background watcher is installed and running.
    Status,
    /// Explain the health of RescueLoop, its event sources, and local state.
    Doctor {
        /// Emit the bounded local health snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Preview or explicitly write a bounded, redacted support bundle.
    Diagnostics {
        #[command(subcommand)]
        action: DiagnosticsAction,
    },
    /// Show the hash-linked lifecycle timeline for one saved incident.
    Timeline {
        incident: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Restart the background watcher.
    Restart,
    /// Stop and remove the background watcher registration.
    Uninstall,
    /// Serve redacted, read-only incident tools over local MCP stdio.
    Mcp,
    /// Monitor OS diagnostic artifacts and persist normalized incidents.
    Watch,
    /// Install, remove, or inspect the per-user background watcher.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Detect installed AI agents and save the selected provider.
    Setup,
    /// Inspect or change enabled event sources.
    Sources {
        #[command(subcommand)]
        action: SourcesAction,
    },
    /// Inspect or safely rebuild the disposable incident index.
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Show recent structured operational events.
    Logs {
        #[arg(long, default_value_t = 100)]
        lines: usize,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        event: Option<String>,
        #[arg(long)]
        correlation_id: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        verify: bool,
        #[arg(long, value_enum, default_value_t = LogOutput::Pretty)]
        output: LogOutput,
    },
    /// Connect to the background detector through the local incident store.
    Console {
        #[arg(long, env = "RESCUELOOP_AI_ENDPOINT")]
        endpoint: Option<String>,
        #[arg(long, env = "RESCUELOOP_AI_TOKEN")]
        token: Option<String>,
        /// Use the line-oriented accessibility/SSH fallback.
        #[arg(long)]
        plain: bool,
    },
    /// Send one saved incident to a user-selected compatible AI endpoint.
    Analyze {
        incident: PathBuf,
        #[arg(long, env = "RESCUELOOP_AI_ENDPOINT")]
        endpoint: String,
        #[arg(long, env = "RESCUELOOP_AI_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Run an action under observation and save an incident on non-success exit.
    Run {
        #[arg(long)]
        record_args: bool,
        executable: PathBuf,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Repeat the exact recorded action and report whether it now succeeds.
    Replay { incident: PathBuf },
    /// Dry-run or explicitly apply one proposed repair, then replay.
    Repair {
        incident: PathBuf,
        analysis: PathBuf,
        #[arg(long, default_value_t = 0)]
        action_index: usize,
        #[arg(long)]
        allow_root: Vec<PathBuf>,
        #[arg(long)]
        approve: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    InstallSystem,
    Uninstall,
    UninstallSystem,
    Status,
}

#[derive(Subcommand)]
enum SourcesAction {
    List,
    Enable { name: String },
    Disable { name: String },
}

#[derive(Subcommand)]
enum IndexAction {
    Status,
    Rebuild,
}

#[derive(Subcommand)]
enum DiagnosticsAction {
    /// Preview bundle contents; write only when --confirm is supplied.
    Export {
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum LogOutput {
    Pretty,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    storage::prepare_state_store(&cli.incident_dir)?;
    let log_guard = logging::init(&cli.incident_dir)?;
    let command = cli.command.as_ref().map_or("start", Command::name);
    info!(
        event = "runtime.started",
        version = env!("CARGO_PKG_VERSION"),
        command,
        pid = std::process::id(),
        "RescueLoop started"
    );
    logging::trigger_test_panic_if_requested();
    let result = run(cli, &log_guard).await;
    match &result {
        Ok(()) => info!(
            event = "runtime.stopped",
            command,
            log_write_errors = log_guard.write_errors(),
            log_export_drops = log_guard.export_drops(),
            "RescueLoop stopped"
        ),
        Err(error) => error!(
            event = "runtime.failed",
            command,
            error = %format!("{error:#}"),
            "RescueLoop failed"
        ),
    }
    metrics::registry().set_log_write_failures(log_guard.write_errors());
    result
}

async fn run(cli: Cli, log_guard: &logging::LogGuard) -> Result<()> {
    match cli.command {
        None | Some(Command::Start) => {
            service::ensure_started(&cli.incident_dir).await?;
            tui::run(cli.incident_dir, None, None, log_guard).await
        }
        Some(Command::Stop) => service::stop().await,
        Some(Command::Status) => service::status().await,
        Some(Command::Doctor { json }) => doctor::run(&cli.incident_dir, log_guard, json).await,
        Some(Command::Diagnostics { action }) => match action {
            DiagnosticsAction::Export { output, confirm } => {
                diagnostics::export(&cli.incident_dir, log_guard, output, confirm).await
            }
        },
        Some(Command::Timeline { incident, json }) => show_timeline(&incident, json).await,
        Some(Command::Restart) => service::restart().await,
        Some(Command::Uninstall) => service::uninstall().await,
        Some(Command::Mcp) => mcp::serve(&cli.incident_dir, log_guard.health()).await,
        Some(Command::Watch) => watcher::run(&cli.incident_dir, log_guard.health()).await,
        Some(Command::Service { action }) => match action {
            ServiceAction::Install => service::install(&cli.incident_dir).await,
            ServiceAction::InstallSystem => service::install_system(&cli.incident_dir).await,
            ServiceAction::Uninstall => service::uninstall().await,
            ServiceAction::UninstallSystem => service::uninstall_system().await,
            ServiceAction::Status => service::status().await,
        },
        Some(Command::Setup) => setup(&cli.incident_dir).await,
        Some(Command::Sources { action }) => sources(&cli.incident_dir, action).await,
        Some(Command::Index { action }) => index_command(&cli.incident_dir, action).await,
        Some(Command::Logs {
            lines,
            follow,
            level,
            event,
            correlation_id,
            since,
            until,
            verify,
            output,
        }) => {
            logging::query(
                &cli.incident_dir,
                logging::LogQuery {
                    lines,
                    follow,
                    level,
                    event,
                    correlation_id,
                    since,
                    until,
                    verify,
                    output: match output {
                        LogOutput::Pretty => logging::LogOutput::Pretty,
                        LogOutput::Json => logging::LogOutput::Json,
                    },
                },
            )
            .await
        }
        Some(Command::Console {
            endpoint,
            token,
            plain,
        }) => {
            if plain {
                console(&cli.incident_dir, endpoint, token).await
            } else {
                tui::run(cli.incident_dir, endpoint, token, log_guard).await
            }
        }
        Some(Command::Analyze {
            incident,
            endpoint,
            token,
            output,
        }) => analyze(&incident, endpoint, token, output.as_deref()).await,
        Some(Command::Run {
            record_args,
            executable,
            args,
        }) => run_supervised(&cli.incident_dir, executable, args, record_args).await,
        Some(Command::Replay { incident }) => replay(&incident).await,
        Some(Command::Repair {
            incident,
            analysis,
            action_index,
            allow_root,
            approve,
        }) => {
            repair(
                &cli.incident_dir,
                &incident,
                &analysis,
                action_index,
                allow_root,
                approve,
            )
            .await
        }
    }
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Status => "status",
            Self::Doctor { .. } => "doctor",
            Self::Diagnostics { .. } => "diagnostics",
            Self::Timeline { .. } => "timeline",
            Self::Restart => "restart",
            Self::Uninstall => "uninstall",
            Self::Mcp => "mcp",
            Self::Watch => "watch",
            Self::Service { .. } => "service",
            Self::Setup => "setup",
            Self::Sources { .. } => "sources",
            Self::Index { .. } => "index",
            Self::Logs { .. } => "logs",
            Self::Console { .. } => "console",
            Self::Analyze { .. } => "analyze",
            Self::Run { .. } => "run",
            Self::Replay { .. } => "replay",
            Self::Repair { .. } => "repair",
        }
    }
}

async fn show_timeline(path: &Path, json: bool) -> Result<()> {
    let incident = incident_store::read_incident_document(path)
        .await
        .context("cannot read incident")?;
    let incident_dir = path.parent().context("incident path has no parent")?;
    let events = timeline::load(incident_dir, &incident).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(());
    }
    println!("Incident {} timeline", incident.id);
    if events.is_empty() {
        println!("No timeline events are available for this legacy incident.");
    }
    for event in events {
        println!(
            "{}  {:?}  {:?}/{:?}  {}  correlation={} ledger={}",
            event.timestamp.to_rfc3339(),
            event.component,
            event.lifecycle_transition,
            event.outcome,
            event.explanation,
            event.correlation_id,
            event.ledger_entry_id,
        );
        if let Some(reason) = event.delay_or_refusal_reason {
            println!("  reason: {reason}");
        }
    }
    Ok(())
}

async fn run_supervised(
    dir: &Path,
    executable: PathBuf,
    args: Vec<String>,
    record_args: bool,
) -> Result<()> {
    match rescueloop_platform::supervise(&executable, &args, record_args).await? {
        None => {
            info!(event = "supervision.passed", "Supervised action succeeded");
            println!("PASSED: original action exited successfully; no incident created.")
        }
        Some(incident) => {
            let (destination, _) = save_incident(dir, &incident).await?;
            info!(event = "supervision.failed", incident_id = %incident.id, kind = ?incident.kind, "Supervised action produced an incident");
            println!("DETECTED: {:?}: {}", incident.kind, incident.message);
            println!("Incident saved to {}", destination.display());
            if record_args {
                println!("Exact replay is available for this incident.");
            } else {
                println!(
                    "Arguments were not stored. Use --record-args only when they contain no secrets and exact replay is needed."
                );
            }
        }
    }
    Ok(())
}

async fn replay(path: &Path) -> Result<()> {
    let incident: Incident =
        serde_json::from_slice(&fs::read(path).await.context("cannot read incident")?)
            .context("invalid incident JSON")?;
    let context = incident
        .launch_context
        .context("incident has no launch context")?;
    let result = {
        let _verification_timer = metrics::registry().timer(metrics::DurationKind::Verification);
        rescueloop_platform::verify_replay(&context).await?
    };
    info!(
        event = "verification.completed",
        incident_id = %incident.id,
        passed = result.passed,
        exit_code = ?result.exit_code,
        duration_ms = result.duration_ms,
        "Replay verification completed"
    );
    println!("{}", serde_json::to_string_pretty(&result)?);
    if result.passed {
        println!("VERIFIED: the exact recorded action now succeeds.");
    } else {
        println!("NOT FIXED: replay still returns a non-success status.");
    }
    Ok(())
}

async fn analyze(
    path: &Path,
    endpoint: String,
    token: Option<String>,
    output: Option<&Path>,
) -> Result<()> {
    let provider = HttpAnalysisProvider::new(endpoint, token);
    let response = analyze_with_provider(path, &provider, output).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if let Some(output) = output {
        println!("Validated analysis saved to {}", output.display());
    }
    println!("No repair was executed. Review the proposal and approve a typed repair separately.");
    Ok(())
}

pub(crate) async fn analyze_with_provider(
    path: &Path,
    provider: &dyn AnalysisProvider,
    output: Option<&Path>,
) -> Result<rescueloop_core::AnalysisResponse> {
    let _analysis_timer = metrics::registry().timer(metrics::DurationKind::Analysis);
    let incident: Incident =
        serde_json::from_slice(&fs::read(path).await.context("cannot read incident")?)
            .context("invalid incident JSON")?;
    let allowed_actions = ALLOWED_ACTIONS
        .iter()
        .copied()
        .filter(|action| cfg!(unix) || *action != "set_permission")
        .map(str::to_string)
        .collect();
    let incident_id = incident.id;
    let request = AnalysisRequest::bounded(incident, allowed_actions);
    let analysis_id = request.analysis_id;
    let analysis_correlation_id = analysis_id.as_uuid();
    info!(
        event = "analysis.started",
        incident_id = %incident_id,
        analysis_id = %analysis_id,
        provider = provider.name(),
        "Analysis started"
    );
    let analysis_span = tracing::info_span!(
        "analysis.run",
        incident_id = %incident_id,
        analysis_id = %analysis_id,
        provider = provider.name(),
    );
    let mut response = match provider.analyze(&request).instrument(analysis_span).await {
        Ok(response) => response,
        Err(error) => {
            if let Some(incident_dir) = path.parent() {
                let _ = timeline::record_with_ids(
                    incident_dir,
                    &request.incident,
                    timeline::EventSpec {
                        correlation_id: Some(analysis_correlation_id),
                        component: rescueloop_ledger::TimelineComponent::Analyzer,
                        transition: rescueloop_ledger::TimelineTransition::Analyzed,
                        outcome: rescueloop_ledger::TimelineOutcome::Failed,
                        explanation: "Analysis did not produce a valid bounded response",
                        reason: Some("provider request or response validation failed"),
                        status: rescueloop_core::IncidentStatus::Investigating,
                        occurred_at: chrono::Utc::now(),
                    },
                    timeline::StageIdentifiers {
                        analysis_id: Some(analysis_id),
                        ..Default::default()
                    },
                )
                .await;
            }
            error!(
                event = "analysis.failed",
                incident_id = %incident_id,
                analysis_id = %analysis_id,
                provider = provider.name(),
                error = %error,
                "Analysis failed"
            );
            return Err(error.into());
        }
    };
    response.analysis_id = Some(analysis_id);
    for action in &mut response.proposed_actions {
        action.plan_id = Some(rescueloop_core::PlanId::new());
    }
    if let Some(output) = output {
        storage::replace_durable(output, &serde_json::to_vec_pretty(&response)?).await?;
    }
    if let Some(incident_dir) = path.parent() {
        timeline::record_with_ids(
            incident_dir,
            &request.incident,
            timeline::EventSpec {
                correlation_id: Some(analysis_correlation_id),
                component: rescueloop_ledger::TimelineComponent::Analyzer,
                transition: rescueloop_ledger::TimelineTransition::Analyzed,
                outcome: rescueloop_ledger::TimelineOutcome::Completed,
                explanation: "Bounded analysis response validated locally",
                reason: None,
                status: rescueloop_core::IncidentStatus::Diagnosed,
                occurred_at: chrono::Utc::now(),
            },
            timeline::StageIdentifiers {
                analysis_id: Some(analysis_id),
                ..Default::default()
            },
        )
        .await?;
        let plan_id = response
            .proposed_actions
            .first()
            .and_then(|action| action.plan_id);
        timeline::record_with_ids(
            incident_dir,
            &request.incident,
            timeline::EventSpec {
                correlation_id: Some(analysis_correlation_id),
                component: rescueloop_ledger::TimelineComponent::Planner,
                transition: rescueloop_ledger::TimelineTransition::PlanProposed,
                outcome: if response.proposed_actions.is_empty() {
                    rescueloop_ledger::TimelineOutcome::Refused
                } else {
                    rescueloop_ledger::TimelineOutcome::Completed
                },
                explanation: if response.proposed_actions.is_empty() {
                    "Analysis produced no safe typed repair plan"
                } else {
                    "Typed repair plan proposed for explicit review"
                },
                reason: response.proposed_actions.is_empty().then_some(
                    if response.needs_more_evidence {
                        "more bounded evidence is required"
                    } else {
                        "no supported safe action was proposed"
                    },
                ),
                status: if response.proposed_actions.is_empty() {
                    rescueloop_core::IncidentStatus::Diagnosed
                } else {
                    rescueloop_core::IncidentStatus::RepairProposed
                },
                occurred_at: chrono::Utc::now(),
            },
            timeline::StageIdentifiers {
                analysis_id: Some(analysis_id),
                plan_id,
                ..Default::default()
            },
        )
        .await?;
    }
    info!(
        event = "analysis.completed",
        incident_id = %incident_id,
        analysis_id = %analysis_id,
        provider = provider.name(),
        proposed_actions = response.proposed_actions.len(),
        needs_more_evidence = response.needs_more_evidence,
        "Analysis completed"
    );
    Ok(response)
}

#[cfg(test)]
mod cli_tests {
    use super::{Cli, Command, DiagnosticsAction};
    use clap::Parser;

    #[test]
    fn no_subcommand_uses_the_combined_start_flow() {
        let cli = Cli::try_parse_from(["rescueloop"]).expect("default CLI should parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_top_level_watcher_lifecycle_commands() {
        for (name, expected) in [
            ("start", "start"),
            ("stop", "stop"),
            ("status", "status"),
            ("doctor", "doctor"),
            ("restart", "restart"),
            ("uninstall", "uninstall"),
        ] {
            let cli =
                Cli::try_parse_from(["rescueloop", name]).expect("lifecycle command should parse");
            assert_eq!(cli.command.as_ref().map(Command::name), Some(expected));
        }
    }

    #[test]
    fn parses_timeline_with_bounded_json_output() {
        let cli = Cli::try_parse_from(["rescueloop", "timeline", "incident.json", "--json"])
            .expect("timeline command should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Timeline {
                incident,
                json: true
            }) if incident == std::path::Path::new("incident.json")
        ));
    }

    #[test]
    fn diagnostics_export_requires_an_explicit_write_flag() {
        let preview = Cli::try_parse_from(["rescueloop", "diagnostics", "export"]).unwrap();
        let Some(Command::Diagnostics {
            action: DiagnosticsAction::Export { confirm, output },
        }) = preview.command
        else {
            panic!("diagnostics export should parse")
        };
        assert!(!confirm);
        assert!(output.is_none());

        let write = Cli::try_parse_from([
            "rescueloop",
            "diagnostics",
            "export",
            "--confirm",
            "--output",
            "support.tar.gz",
        ])
        .unwrap();
        let Some(Command::Diagnostics {
            action: DiagnosticsAction::Export { confirm, output },
        }) = write.command
        else {
            panic!("confirmed diagnostics export should parse")
        };
        assert!(confirm);
        assert_eq!(
            output.as_deref(),
            Some(std::path::Path::new("support.tar.gz"))
        );
    }
}

#[cfg(test)]
mod timeline_flow_tests {
    use super::*;
    use rescueloop_core::{
        AnalysisError, AnalysisResponse, Evidence, IncidentKind, ProposedAction,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    struct FixedProvider;

    #[async_trait::async_trait]
    impl AnalysisProvider for FixedProvider {
        fn name(&self) -> &str {
            "fixture"
        }

        async fn analyze(
            &self,
            _request: &AnalysisRequest,
        ) -> std::result::Result<AnalysisResponse, AnalysisError> {
            Ok(AnalysisResponse {
                summary: "Bounded fixture analysis".into(),
                hypotheses: Vec::new(),
                proposed_actions: vec![ProposedAction {
                    action_type: "restart_service".into(),
                    reason: "Use the evidence-bound service identifier".into(),
                    parameters: serde_json::json!({"service_id": "fixture"}),
                    reversible: true,
                    plan_id: None,
                }],
                needs_more_evidence: false,
                analysis_id: None,
            })
        }
    }

    #[tokio::test]
    async fn repeated_analysis_attempts_have_distinct_correlations() {
        let root = std::env::temp_dir().join(format!("rescueloop-analysis-{}", Uuid::new_v4()));
        let incident_dir = root.join("incidents");
        tokio::fs::create_dir_all(&incident_dir).await.unwrap();
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
        let path = incident_dir.join(format!("{}.json", incident.id));
        tokio::fs::write(&path, serde_json::to_vec(&incident).unwrap())
            .await
            .unwrap();
        timeline::ensure_initial(&incident_dir, &incident)
            .await
            .unwrap();
        let analysis_path = root.join("analysis.json");
        let first = analyze_with_provider(&path, &FixedProvider, Some(&analysis_path))
            .await
            .unwrap();
        let second = analyze_with_provider(&path, &FixedProvider, Some(&analysis_path))
            .await
            .unwrap();
        assert!(first.analysis_id.is_some());
        assert!(first.proposed_actions[0].plan_id.is_some());
        assert_ne!(first.analysis_id, second.analysis_id);
        assert_ne!(
            first.proposed_actions[0].plan_id,
            second.proposed_actions[0].plan_id
        );
        let saved: AnalysisResponse =
            serde_json::from_slice(&tokio::fs::read(&analysis_path).await.unwrap()).unwrap();
        assert_eq!(saved.analysis_id, second.analysis_id);
        assert_eq!(
            saved.proposed_actions[0].plan_id,
            second.proposed_actions[0].plan_id
        );
        let events = timeline::load(&incident_dir, &incident).await.unwrap();
        let analyzed = events
            .iter()
            .filter(|event| {
                event.lifecycle_transition == rescueloop_ledger::TimelineTransition::Analyzed
            })
            .collect::<Vec<_>>();
        assert_eq!(analyzed.len(), 2);
        assert_ne!(analyzed[0].correlation_id, analyzed[1].correlation_id);
        for event in analyzed {
            assert!(event.analysis_id.is_some());
            assert!(events.iter().any(|candidate| {
                candidate.correlation_id == event.correlation_id
                    && candidate.lifecycle_transition
                        == rescueloop_ledger::TimelineTransition::PlanProposed
                    && candidate.analysis_id == event.analysis_id
                    && candidate.plan_id.is_some()
            }));
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
