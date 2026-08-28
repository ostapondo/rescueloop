use super::{App, UiState};
use crate::local_timestamp;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use rescueloop_core::{AnalysisResponse, Incident};
use serde_json::Value;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let footer_height = if area.width >= 170 { 3 } else { 5 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Min(16),
            Constraint::Length(footer_height),
        ])
        .split(area);

    render_header(frame, app, chunks[0]);
    render_health(frame, app, chunks[1]);

    let incident_height = if area.height >= 34 {
        Constraint::Percentage(42)
    } else {
        Constraint::Percentage(48)
    };
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([incident_height, Constraint::Min(9)])
        .split(chunks[2]);
    render_incidents(frame, app, body[0]);
    render_workspace(frame, app, body[1]);
    render_footer(frame, app, chunks[3]);
}

fn render_health(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let healthy = app
        .health
        .sources
        .iter()
        .filter(|source| source.state == crate::watch_health::SourceState::Healthy)
        .count();
    let degraded = app
        .health
        .sources
        .iter()
        .filter(|source| source.state == crate::watch_health::SourceState::Degraded)
        .count();
    let disconnected = app
        .health
        .sources
        .iter()
        .filter(|source| source.state == crate::watch_health::SourceState::Disconnected)
        .count();
    let watcher = app
        .health
        .checks
        .iter()
        .find(|check| check.name == "watcher");
    let state = watcher.map_or("UNKNOWN", |check| check.state.label());
    let queue = if app.health.queue_capacity == 0 {
        "unavailable".into()
    } else {
        format!("{}/{}", app.health.queue_depth, app.health.queue_capacity)
    };
    let text = format!(
        "Watcher: {state}  Sources: {healthy} healthy / {degraded} degraded / {disconnected} disconnected\nQueue: {queue}  Journal: {}  Received: {}  Persisted: {}  Grouped: {}  Deduplicated: {}",
        app.health.journal_pending,
        app.health.received,
        app.health.persisted,
        app.health.grouped,
        app.health.deduplicated
    );
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Self-health · `rescueloop doctor` for details "),
        ),
        area,
    );
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " RescueLoop ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  LIVE  ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("•", Style::default().fg(Color::Yellow)),
        Span::raw(format!("  AI: {}", app.agent_name)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, area);
}

fn render_incidents(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app
        .incidents
        .iter()
        .map(|(incident, _)| {
            Row::new(vec![
                Cell::from(
                    incident
                        .application
                        .as_deref()
                        .unwrap_or("Unknown application"),
                ),
                Cell::from(format!("{:?}", incident.kind)),
                Cell::from(incident_source_label(incident)),
                Cell::from(format!("×{}", incident.occurrence_count)),
                Cell::from(local_timestamp(
                    incident.last_observed_at.unwrap_or(incident.observed_at),
                )),
                Cell::from(format!("{:?}", incident.status)),
            ])
        })
        .collect::<Vec<_>>();
    let header = Row::new([
        "APPLICATION",
        "PROBLEM",
        "SOURCE",
        "COUNT",
        "LOCAL TIME",
        "STATUS",
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .bottom_margin(1);
    let table = Table::new(
        rows,
        [
            Constraint::Fill(5),
            Constraint::Fill(3),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(20),
            Constraint::Length(18),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(format!(
                " {} incidents ({}) ",
                if app.show_history {
                    "History"
                } else {
                    "Active"
                },
                app.incidents.len()
            ))
            .borders(Borders::ALL),
    )
    .row_highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
    .highlight_symbol("▶ ")
    .highlight_spacing(HighlightSpacing::Always);
    let mut state =
        TableState::default().with_selected((!app.incidents.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_workspace(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.show_timeline {
        render_timeline(frame, app, area);
        return;
    }
    if app.show_health {
        render_doctor(frame, app, area);
        return;
    }
    if let UiState::Message(message) = &app.state {
        render_panel(frame, area, " Status ", message.clone(), Color::Yellow);
        return;
    }
    let Some((incident, _)) = app.incidents.get(app.selected) else {
        render_panel(
            frame,
            area,
            " Waiting for an incident ",
            "RescueLoop is watching for objective failures.".into(),
            MUTED,
        );
        return;
    };

    let evidence = evidence_text(incident, app.show_details);
    let Some(analysis) = &app.analysis else {
        let title = if app.show_details {
            " Evidence · [Enter] Collapse "
        } else {
            " Evidence · [Enter] Expand "
        };
        render_panel(frame, area, title, evidence, ACCENT);
        return;
    };

    if area.width >= 112 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);
        render_panel(frame, columns[0], " Evidence ", evidence, ACCENT);
        render_analysis_column(frame, analysis, app.show_repair, columns[1]);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        render_panel(frame, rows[0], " Evidence ", evidence, ACCENT);
        render_analysis_column(frame, analysis, app.show_repair, rows[1]);
    }
}

fn render_timeline(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut text = String::new();
    if app.timeline.is_empty() {
        text.push_str("No timeline events are available for this incident.\n");
    }
    for event in &app.timeline {
        text.push_str(&format!(
            "{}  {:?}  {:?}/{:?}\n  {}\n  correlation={} ledger={}\n",
            event.timestamp.to_rfc3339(),
            event.component,
            event.lifecycle_transition,
            event.outcome,
            event.explanation,
            event.correlation_id,
            event.ledger_entry_id,
        ));
        if let Some(reason) = &event.delay_or_refusal_reason {
            text.push_str(&format!("  reason: {reason}\n"));
        }
    }
    render_panel(
        frame,
        area,
        " Incident timeline · [T/Esc] Close ",
        text,
        ACCENT,
    );
}

fn render_doctor(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut text = String::from("COMPONENTS\n");
    for check in &app.health.checks {
        text.push_str(&format!(
            "{:<22} {:<12} {}\n",
            check.name,
            check.state.label(),
            check.detail
        ));
    }
    text.push_str("\nEVENT SOURCES\n");
    if app.health.sources.is_empty() {
        text.push_str("No watcher health snapshot is available yet.\n");
    }
    for source in &app.health.sources {
        text.push_str(&format!(
            "{:<20} {:<12} read={} dropped={} dedup={} reconnects={} backoff={}ms last={}\n",
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
        ));
    }
    text.push_str(&format!(
        "\nPIPELINE\nqueue={}/{} journal={} received={} persisted={} grouped={} deduplicated={} uptime={} last_shutdown={}",
        app.health.queue_depth,
        app.health.queue_capacity,
        app.health.journal_pending,
        app.health.received,
        app.health.persisted,
        app.health.grouped,
        app.health.deduplicated,
        app.health.watcher_uptime_seconds.map_or_else(|| "unknown".into(), |value| format!("{value}s")),
        app.health.last_shutdown_reason.as_deref().unwrap_or("none recorded")
    ));
    text.push_str(&format!(
        "\n\nLOCAL METRICS · export disabled\nreconnects={} queue={} rollbacks={} log_failures={} index_rebuilds={} journal={}\npersist={}us/{} grouping={}us/{} analysis={}us/{} repair={}us/{} verification={}us/{}",
        app.health.metrics.source_reconnects_total,
        app.health.metrics.queue_depth,
        app.health.metrics.rollback_total,
        app.health.metrics.log_write_failures_total,
        app.health.metrics.index_rebuild_total,
        app.health.metrics.journal_pending_count,
        app.health.metrics.incident_persist_duration.last_micros,
        app.health.metrics.incident_persist_duration.count,
        app.health.metrics.incident_grouping_duration.last_micros,
        app.health.metrics.incident_grouping_duration.count,
        app.health.metrics.analysis_duration.last_micros,
        app.health.metrics.analysis_duration.count,
        app.health.metrics.repair_duration.last_micros,
        app.health.metrics.repair_duration.count,
        app.health.metrics.verification_duration.last_micros,
        app.health.metrics.verification_duration.count,
    ));
    render_panel(frame, area, " Self-health · [V/Esc] Close ", text, ACCENT);
}

fn render_analysis_column(
    frame: &mut Frame<'_>,
    analysis: &AnalysisResponse,
    show_repair: bool,
    area: Rect,
) {
    if analysis.proposed_actions.is_empty() {
        let outcome = if analysis.needs_more_evidence {
            "\n\nMore evidence is required. Nothing was changed."
        } else {
            "\n\nNo applicable repair was found. Nothing was changed."
        };
        render_panel(
            frame,
            area,
            " Analysis · saved ",
            format!("{}{}", analysis.summary, outcome),
            ACCENT,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);
    render_panel(
        frame,
        rows[0],
        " Analysis · saved ",
        analysis.summary.clone(),
        ACCENT,
    );
    let proposal = &analysis.proposed_actions[0];
    let mut plan = format!(
        "1. {}\n2. Verify by replaying the original failure\n3. Roll back if verification fails",
        action_label(&proposal.action_type)
    );
    if show_repair {
        plan.push_str(&format!(
            "\n\nReason: {}\nParameters: {}\nReversible: {}",
            proposal.reason,
            compact_value(&proposal.parameters),
            if proposal.reversible { "yes" } else { "no" }
        ));
    } else {
        plan.push_str("\n\n[R] Review exact plan");
    }
    render_panel(
        frame,
        rows[1],
        if show_repair {
            " Proposed repair · reviewed "
        } else {
            " Proposed repair "
        },
        plan,
        Color::Yellow,
    );
}

fn render_panel(frame: &mut Frame<'_>, area: Rect, title: &str, text: String, color: Color) {
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(Span::styled(
                    title,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let footer = match app.state {
        UiState::ConfirmAnalysis {
            replace_saved: false,
        } => "Send scrubbed evidence to AI?  [y] Yes  [n] No".to_string(),
        UiState::ConfirmAnalysis {
            replace_saved: true,
        } => "Replace saved analysis?  [y] Re-analyze  [n] Keep saved".to_string(),
        UiState::ConfirmRepair => {
            "Apply this reviewed repair, verify it, and roll back on failure?  [y] Apply  [n] Cancel"
                .to_string()
        }
        UiState::ConfirmQuit => {
            "Disconnect? The background watcher stays active.  [y] Exit  [n] Stay".to_string()
        }
        UiState::Analyzing { started } => progress("AI is analyzing bounded evidence", started),
        UiState::Repairing { started } => progress("Applying, verifying, and protecting rollback", started),
        UiState::Gathering { started } => progress("Reproducing the failure and gathering evidence", started),
        _ => ready_footer(app, area.width),
    };
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn ready_footer(app: &App, width: u16) -> String {
    let evidence_action = if app.show_details {
        "[Enter] Collapse evidence"
    } else {
        "[Enter] Expand evidence"
    };
    let analysis_action = if app.analysis.is_some() {
        ""
    } else {
        "[A] Analyze"
    };
    let repair_action = if app
        .analysis
        .as_ref()
        .is_some_and(|value| !value.proposed_actions.is_empty())
    {
        if app.show_repair {
            "[R] Request approval"
        } else {
            "[R] Review plan"
        }
    } else if app
        .analysis
        .as_ref()
        .is_some_and(|value| value.needs_more_evidence && value.proposed_actions.is_empty())
    {
        "[G] Gather evidence"
    } else {
        ""
    };
    let history = if app.show_history {
        "[H] Active incidents"
    } else {
        "[H] History"
    };
    let refresh = if app.analysis.is_some() {
        "[U] Re-analyze"
    } else {
        ""
    };
    let first = format!(
        " {:<17}{:<29}{:<16}{:<20}{}",
        "[↑↓] Select", evidence_action, analysis_action, refresh, repair_action
    );
    let second = format!(
        " {:<17}{:<21}{:<21}{:<21}{}",
        "[D] Dismiss", history, "[T] Timeline", "[V] Self-health", "[Q] Disconnect"
    );
    if width >= 170 {
        format!("{first}  {second}")
    } else {
        format!(
            " {:<17}{}\n {:<21}{:<20}{}\n{}",
            "[↑↓] Select", evidence_action, analysis_action, refresh, repair_action, second
        )
    }
}

fn progress(label: &str, started: std::time::Instant) -> String {
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let index = (started.elapsed().as_millis() / 100) as usize % frames.len();
    format!(
        " {} {}… {:.1}s ",
        frames[index],
        label,
        started.elapsed().as_secs_f32()
    )
}

fn evidence_text(incident: &Incident, expanded: bool) -> String {
    let mut lines = vec![
        incident.message.to_string(),
        String::new(),
        format!("Source       {}", incident_source_label(incident)),
        format!("Failure      {:?}", incident.kind),
        format!("Status       {:?}", incident.status),
        format!("Confidence   {:?}", incident.confidence),
        format!("Occurrences  {}", incident.occurrence_count),
        format!(
            "Last seen    {}",
            local_timestamp(incident.last_observed_at.unwrap_or(incident.observed_at))
        ),
    ];
    for evidence in &incident.evidence {
        lines.push(String::new());
        lines.push(format!("• {}", evidence.summary));
        if expanded {
            for (key, value) in &evidence.fields {
                lines.push(format!("  {key}: {}", compact_value(value)));
            }
        }
    }
    lines.join("\n")
}

fn action_label(action_type: &str) -> String {
    match action_type {
        "quarantine_path" => "Quarantine the evidence-bound path".into(),
        "regenerate_cache" => "Regenerate the evidence-bound cache".into(),
        "patch_json_config" => "Apply the reviewed configuration change".into(),
        "set_permission" => "Restore the reviewed permissions".into(),
        "restart_service" => "Restart the exact evidence-bound service".into(),
        "restart_container" => "Restart the exact evidence-bound container".into(),
        other => other.replace('_', " "),
    }
}

fn compact_value(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    };
    const MAX_CHARS: usize = 180;
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut bounded = text.chars().take(MAX_CHARS).collect::<String>();
    bounded.push('…');
    bounded
}

fn incident_source_label(incident: &Incident) -> String {
    if let Some(engine) = incident
        .evidence
        .iter()
        .find_map(|evidence| evidence.fields.get("engine").and_then(Value::as_str))
    {
        let mut characters = engine.chars();
        return characters
            .next()
            .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
            .unwrap_or_else(|| "Container".into());
    }
    let source = incident
        .evidence
        .first()
        .map(|evidence| evidence.source.as_str())
        .unwrap_or_default();
    if source.starts_with("macos") {
        "macOS".into()
    } else if source.starts_with("windows") {
        "Windows".into()
    } else if source == "supervised-process" {
        "Process".into()
    } else {
        "System".into()
    }
}

#[cfg(test)]
mod tests {
    use super::{action_label, compact_value};
    use crate::tui::{App, UiState, draw};
    use ratatui::{Terminal, backend::TestBackend};
    use rescueloop_core::{AnalysisResponse, Evidence, Hypothesis, Incident, ProposedAction};
    use serde_json::json;
    use std::{collections::BTreeMap, path::PathBuf};

    #[test]
    fn labels_typed_actions_without_exposing_a_shell_shape() {
        assert_eq!(
            action_label("restart_container"),
            "Restart the exact evidence-bound container"
        );
        assert_eq!(
            action_label("patch_json_config"),
            "Apply the reviewed configuration change"
        );
    }

    #[test]
    fn bounds_rendered_parameter_values() {
        let rendered = compact_value(&json!("x".repeat(300)));
        assert_eq!(rendered.chars().count(), 181);
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn renders_incidents_and_reviewed_plan_at_wide_and_narrow_sizes() {
        let mut incident = Incident::detected(
            "test",
            rescueloop_core::IncidentKind::RestartLoop,
            "Container restarted repeatedly",
            Evidence {
                source: "container-event".into(),
                summary: "Docker reported an out-of-memory exit".into(),
                artifact: None,
                fields: BTreeMap::from([
                    ("engine".into(), json!("docker")),
                    ("exit_code".into(), json!(137)),
                ]),
            },
        );
        incident.application = Some("checkout-api".into());
        let mut app = App {
            incidents: vec![(incident, PathBuf::from("incident.json"))],
            selected: 0,
            show_details: true,
            show_repair: true,
            state: UiState::Ready,
            analysis: Some(AnalysisResponse {
                summary: "The container exceeded its memory limit.".into(),
                hypotheses: vec![Hypothesis {
                    cause: "memory limit".into(),
                    confidence: 0.9,
                    evidence_indexes: vec![0],
                }],
                proposed_actions: vec![ProposedAction {
                    action_type: "restart_container".into(),
                    reason: "Restart the exact unhealthy container.".into(),
                    parameters: json!({"engine": "docker", "container_id": "fixture"}),
                    reversible: true,
                    plan_id: None,
                }],
                needs_more_evidence: false,
                analysis_id: None,
            }),
            agent_name: "fixture-agent".into(),
            show_history: false,
            show_health: false,
            show_timeline: false,
            timeline: Vec::new(),
            health: crate::doctor::DoctorSnapshot {
                version: "0.0.1".into(),
                watcher_uptime_seconds: Some(10),
                last_shutdown_reason: None,
                checks: vec![crate::doctor::Check {
                    name: "watcher".into(),
                    state: crate::doctor::HealthState::Healthy,
                    detail: "running".into(),
                }],
                sources: Vec::new(),
                received: 1,
                persisted: 1,
                grouped: 0,
                deduplicated: 0,
                queue_depth: 0,
                queue_capacity: 256,
                journal_pending: 0,
                metrics: crate::metrics::MetricsSnapshot::default(),
            },
        };

        for (width, height) in [(180, 48), (96, 32)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("Active incidents"));
            assert!(rendered.contains("Self-health"));
            assert!(rendered.contains("Evidence"));
            assert!(rendered.contains("Analysis · saved"));
            assert!(rendered.contains("Proposed repair"));
            assert!(rendered.contains("Request approval"));
            assert!(rendered.contains("Collapse evidence"));
            assert!(!rendered.contains("[A] Saved"));
        }

        app.show_health = true;
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("COMPONENTS"));
        assert!(rendered.contains("EVENT SOURCES"));
        assert!(rendered.contains("PIPELINE"));

        app.show_health = false;
        app.show_timeline = true;
        app.timeline = vec![crate::timeline::TimelineEvent {
            timestamp: chrono::Utc::now(),
            correlation_id: uuid::Uuid::new_v4(),
            observation_id: None,
            incident_id: None,
            occurrence_id: None,
            analysis_id: None,
            plan_id: None,
            repair_transaction_id: None,
            verification_id: None,
            component: rescueloop_ledger::TimelineComponent::Approval,
            lifecycle_transition: rescueloop_ledger::TimelineTransition::Approved,
            outcome: rescueloop_ledger::TimelineOutcome::Refused,
            explanation: "Repair stopped before mutation".into(),
            ledger_entry_id: uuid::Uuid::new_v4(),
            delay_or_refusal_reason: Some("explicit local approval was not provided".into()),
        }];
        let backend = TestBackend::new(96, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Incident timeline"));
        assert!(rendered.contains("Repair stopped before mutation"));
        assert!(rendered.contains("explicit local approval was not provided"));
        assert!(rendered.contains("ledger="));
    }
}
