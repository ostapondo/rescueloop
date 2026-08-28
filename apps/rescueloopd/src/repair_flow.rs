use anyhow::{Context, Result};
use rescueloop_core::Incident;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::Instrument;
use uuid::Uuid;

use crate::incident_store::{ledger_path, record_incident_status};

pub(crate) async fn repair(
    incident_dir: &Path,
    incident_path: &Path,
    analysis_path: &Path,
    action_index: usize,
    allowed_roots: Vec<PathBuf>,
    approved: bool,
) -> Result<()> {
    repair_impl(
        incident_dir,
        incident_path,
        analysis_path,
        action_index,
        allowed_roots,
        approved,
        true,
    )
    .await
}

pub(crate) async fn repair_silent(
    incident_dir: &Path,
    incident_path: &Path,
    analysis_path: &Path,
    action_index: usize,
    allowed_roots: Vec<PathBuf>,
    approved: bool,
) -> Result<()> {
    repair_impl(
        incident_dir,
        incident_path,
        analysis_path,
        action_index,
        allowed_roots,
        approved,
        false,
    )
    .await
}

async fn repair_impl(
    incident_dir: &Path,
    incident_path: &Path,
    analysis_path: &Path,
    action_index: usize,
    allowed_roots: Vec<PathBuf>,
    approved: bool,
    verbose: bool,
) -> Result<()> {
    macro_rules! report {
        ($($argument:tt)*) => {
            if verbose {
                println!($($argument)*);
            }
        };
    }
    let incident: Incident = serde_json::from_slice(&fs::read(incident_path).await?)?;
    let analysis: rescueloop_core::AnalysisResponse =
        serde_json::from_slice(&fs::read(analysis_path).await?)?;
    let proposal = analysis
        .proposed_actions
        .get(action_index)
        .context("action index is out of range")?;
    let analysis_id = analysis.analysis_id.unwrap_or_default();
    let plan_id = proposal.plan_id.unwrap_or_default();
    let timeline_correlation_id = plan_id.as_uuid();
    let mut repair_ids = crate::timeline::StageIdentifiers {
        analysis_id: Some(analysis_id),
        plan_id: Some(plan_id),
        ..Default::default()
    };
    record_timeline(
        incident_dir,
        &incident,
        timeline_correlation_id,
        repair_ids,
        rescueloop_ledger::TimelineComponent::Planner,
        rescueloop_ledger::TimelineTransition::PlanProposed,
        rescueloop_ledger::TimelineOutcome::Completed,
        "Typed repair plan selected for review",
        None,
        rescueloop_core::IncidentStatus::RepairProposed,
    )
    .await?;
    tracing::info!(
        event = "repair.planned",
        incident_id = %incident.id,
        analysis_id = %analysis_id,
        plan_id = %plan_id,
        action_type = proposal.action_type,
        approved,
        "Repair proposal compiled"
    );
    if let Some(action) = rescueloop_repair::compile_operational(proposal)? {
        report!("DRY RUN: {}", serde_json::to_string_pretty(&action)?);
        if !approved {
            record_timeline(
                incident_dir,
                &incident,
                timeline_correlation_id,
                repair_ids,
                rescueloop_ledger::TimelineComponent::Approval,
                rescueloop_ledger::TimelineTransition::Approved,
                rescueloop_ledger::TimelineOutcome::Refused,
                "Repair stopped before mutation",
                Some("explicit local approval was not provided"),
                rescueloop_core::IncidentStatus::RepairProposed,
            )
            .await?;
            tracing::info!(event = "repair.dry_run", incident_id = %incident.id, action_type = proposal.action_type, "Operational repair reviewed without execution");
            report!("No changes made. Approve this exact operational target to execute.");
            return Ok(());
        }
        let _repair_timer = crate::metrics::registry().timer(crate::metrics::DurationKind::Repair);
        let _verification_timer =
            crate::metrics::registry().timer(crate::metrics::DurationKind::Verification);
        let verification_id = rescueloop_core::VerificationId::new();
        repair_ids.verification_id = Some(verification_id);
        let repair_transaction_id = rescueloop_core::RepairTransactionId::new();
        repair_ids.repair_transaction_id = Some(repair_transaction_id);
        let target_id = match &action {
            rescueloop_repair::OperationalAction::RestartContainer { container_id, .. } => {
                container_id.clone()
            }
            rescueloop_repair::OperationalAction::RestartService { service_id } => {
                service_id.clone()
            }
        };
        let evidenced = incident.evidence.iter().any(|evidence| {
            evidence
                .fields
                .values()
                .any(|value| value.as_str() == Some(target_id.as_str()))
        });
        if !evidenced {
            record_timeline(
                incident_dir,
                &incident,
                timeline_correlation_id,
                repair_ids,
                rescueloop_ledger::TimelineComponent::Approval,
                rescueloop_ledger::TimelineTransition::Approved,
                rescueloop_ledger::TimelineOutcome::Refused,
                "Operational repair target failed local evidence binding",
                Some("target was not present in bounded incident evidence"),
                rescueloop_core::IncidentStatus::RepairProposed,
            )
            .await?;
            anyhow::bail!("operational target is not present in incident evidence")
        }
        record_timeline(
            incident_dir,
            &incident,
            timeline_correlation_id,
            repair_ids,
            rescueloop_ledger::TimelineComponent::Approval,
            rescueloop_ledger::TimelineTransition::Approved,
            rescueloop_ledger::TimelineOutcome::Completed,
            "Exact reviewed operational repair approved locally",
            None,
            rescueloop_core::IncidentStatus::RepairProposed,
        )
        .await?;
        record_incident_status(
            incident_dir,
            &incident,
            rescueloop_core::IncidentStatus::RepairApplied,
            None,
        )
        .await?;
        let receipt = match rescueloop_repair::execute_operational(
            action,
            &target_id,
            analysis_id,
            plan_id,
            repair_transaction_id,
            verification_id,
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                record_timeline(
                    incident_dir,
                    &incident,
                    timeline_correlation_id,
                    repair_ids,
                    rescueloop_ledger::TimelineComponent::Repair,
                    rescueloop_ledger::TimelineTransition::Applied,
                    rescueloop_ledger::TimelineOutcome::Failed,
                    "Approved operational repair could not be applied",
                    Some("typed action execution failed"),
                    rescueloop_core::IncidentStatus::RepairProposed,
                )
                .await?;
                return Err(error);
            }
        };
        debug_assert_eq!(receipt.id, repair_transaction_id);
        record_timeline(
            incident_dir,
            &incident,
            timeline_correlation_id,
            repair_ids,
            rescueloop_ledger::TimelineComponent::Repair,
            rescueloop_ledger::TimelineTransition::Applied,
            rescueloop_ledger::TimelineOutcome::Completed,
            "Approved operational repair action applied",
            None,
            rescueloop_core::IncidentStatus::RepairApplied,
        )
        .await?;
        if receipt.rolled_back {
            crate::metrics::registry().rollback();
        }
        tracing::info!(
            event = "repair.executed",
            incident_id = %incident.id,
            analysis_id = %analysis_id,
            plan_id = %plan_id,
            repair_transaction_id = %receipt.id,
            verification_id = %verification_id,
            action_type = proposal.action_type,
            verified = receipt.verified,
            rolled_back = receipt.rolled_back,
            "Operational repair executed"
        );
        record_incident_status(
            incident_dir,
            &incident,
            rescueloop_core::IncidentStatus::VerificationPending,
            None,
        )
        .await?;
        let status = if receipt.verified {
            rescueloop_core::IncidentStatus::VerifiedFixed
        } else if receipt.rolled_back {
            rescueloop_core::IncidentStatus::RolledBack
        } else {
            rescueloop_core::IncidentStatus::VerificationFailed
        };
        record_timeline(
            incident_dir,
            &incident,
            timeline_correlation_id,
            repair_ids,
            rescueloop_ledger::TimelineComponent::Verifier,
            rescueloop_ledger::TimelineTransition::Verified,
            if receipt.verified {
                rescueloop_ledger::TimelineOutcome::Completed
            } else {
                rescueloop_ledger::TimelineOutcome::Failed
            },
            if receipt.verified {
                "Operational repair passed its bounded verification"
            } else {
                "Operational repair did not pass bounded verification"
            },
            (!receipt.verified).then_some("post-repair objective check failed"),
            status.clone(),
        )
        .await?;
        record_timeline(
            incident_dir,
            &incident,
            timeline_correlation_id,
            repair_ids,
            rescueloop_ledger::TimelineComponent::Ledger,
            if receipt.verified {
                rescueloop_ledger::TimelineTransition::Committed
            } else {
                rescueloop_ledger::TimelineTransition::RolledBack
            },
            if receipt.verified || receipt.rolled_back {
                rescueloop_ledger::TimelineOutcome::Completed
            } else {
                rescueloop_ledger::TimelineOutcome::Failed
            },
            if receipt.verified {
                "Verified operational repair committed"
            } else if receipt.rolled_back {
                "Failed operational repair restored its prior state"
            } else {
                "Operational repair could not confirm prior-state restoration"
            },
            (!receipt.verified).then_some(if receipt.rolled_back {
                "verification failed"
            } else {
                "verification failed and rollback was unavailable"
            }),
            status.clone(),
        )
        .await?;
        let transaction_root = incident_dir
            .parent()
            .unwrap_or(incident_dir)
            .join("transactions")
            .join(receipt.id.to_string());
        fs::create_dir_all(&transaction_root).await?;
        let receipt_path = transaction_root.join("operational-receipt.json");
        crate::storage::replace_durable(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)
            .await?;
        record_incident_status(
            incident_dir,
            &incident,
            status,
            Some(serde_json::to_value(&receipt)?),
        )
        .await?;
        if !receipt.verified {
            anyhow::bail!("operational repair failed verification")
        }
        report!(
            "VERIFIED operational repair. Receipt: {}",
            receipt_path.display()
        );
        return Ok(());
    }
    let plan = rescueloop_repair::compile(proposal)?;
    let proposed_target = std::fs::canonicalize(plan.action.target()).with_context(|| {
        format!(
            "repair target does not exist: {}",
            plan.action.target().display()
        )
    })?;
    let target_is_evidenced = incident.evidence.iter().any(|evidence| {
        evidence
            .artifact
            .as_ref()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .is_some_and(|path| path == proposed_target)
    });
    if !target_is_evidenced {
        anyhow::bail!(
            "filesystem repair target is not the exact artifact recorded in incident evidence"
        )
    }
    let policy = rescueloop_repair::ScopePolicy::new(allowed_roots)?;
    let transaction_root = incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("transactions");
    let mut transaction = rescueloop_repair::prepare(&plan, &policy, &transaction_root).await?;
    transaction.analysis_id = Some(analysis_id);
    transaction.plan_id = Some(plan_id);
    repair_ids.repair_transaction_id = Some(transaction.id);
    report!("DRY RUN: {}", serde_json::to_string_pretty(&transaction)?);
    if !approved {
        record_timeline(
            incident_dir,
            &incident,
            timeline_correlation_id,
            repair_ids,
            rescueloop_ledger::TimelineComponent::Approval,
            rescueloop_ledger::TimelineTransition::Approved,
            rescueloop_ledger::TimelineOutcome::Refused,
            "Repair stopped before mutation",
            Some("explicit local approval was not provided"),
            rescueloop_core::IncidentStatus::RepairProposed,
        )
        .await?;
        tracing::info!(event = "repair.dry_run", incident_id = %incident.id, action_type = proposal.action_type, "Filesystem repair reviewed without execution");
        report!("No changes made. Review the exact target and repeat with --approve to execute.");
        return Ok(());
    }
    record_timeline(
        incident_dir,
        &incident,
        timeline_correlation_id,
        repair_ids,
        rescueloop_ledger::TimelineComponent::Approval,
        rescueloop_ledger::TimelineTransition::Approved,
        rescueloop_ledger::TimelineOutcome::Completed,
        "Exact reviewed filesystem repair approved locally",
        None,
        rescueloop_core::IncidentStatus::RepairProposed,
    )
    .await?;
    let launch_context = incident
        .launch_context
        .clone()
        .context("verified repair requires an exact recorded launch context")?;
    {
        let _repair_timer = crate::metrics::registry().timer(crate::metrics::DurationKind::Repair);
        if let Err(error) = rescueloop_repair::apply(&mut transaction).await {
            record_timeline(
                incident_dir,
                &incident,
                timeline_correlation_id,
                repair_ids,
                rescueloop_ledger::TimelineComponent::Repair,
                rescueloop_ledger::TimelineTransition::Applied,
                rescueloop_ledger::TimelineOutcome::Failed,
                "Approved filesystem repair could not be applied",
                Some("reversible transaction apply failed"),
                rescueloop_core::IncidentStatus::RepairProposed,
            )
            .await?;
            return Err(error);
        }
    }
    record_timeline(
        incident_dir,
        &incident,
        timeline_correlation_id,
        repair_ids,
        rescueloop_ledger::TimelineComponent::Repair,
        rescueloop_ledger::TimelineTransition::Applied,
        rescueloop_ledger::TimelineOutcome::Completed,
        "Approved reversible filesystem repair applied",
        None,
        rescueloop_core::IncidentStatus::RepairApplied,
    )
    .await?;
    tracing::info!(
        event = "repair.applied",
        incident_id = %incident.id,
        analysis_id = %analysis_id,
        plan_id = %plan_id,
        repair_transaction_id = %transaction.id,
        action_type = proposal.action_type,
        "Repair transaction applied"
    );
    record_incident_status(
        incident_dir,
        &incident,
        rescueloop_core::IncidentStatus::RepairApplied,
        None,
    )
    .await?;
    rescueloop_repair::persist(&transaction, &transaction_root).await?;
    report!(
        "APPLIED: backup created at {}",
        transaction.backup.display()
    );

    record_incident_status(
        incident_dir,
        &incident,
        rescueloop_core::IncidentStatus::VerificationPending,
        None,
    )
    .await?;
    let verification_id = rescueloop_core::VerificationId::new();
    transaction.verification_id = Some(verification_id);
    repair_ids.verification_id = Some(verification_id);
    let replay = {
        let _verification_timer =
            crate::metrics::registry().timer(crate::metrics::DurationKind::Verification);
        rescueloop_platform::verify_replay(&launch_context)
            .instrument(tracing::info_span!(
                "verification.run",
                incident_id = %incident.incident_id(),
                analysis_id = %analysis_id,
                plan_id = %plan_id,
                repair_transaction_id = %transaction.id,
                verification_id = %verification_id,
            ))
            .await
    };
    match replay {
        Ok(result) if result.passed => {
            record_timeline(
                incident_dir,
                &incident,
                timeline_correlation_id,
                repair_ids,
                rescueloop_ledger::TimelineComponent::Verifier,
                rescueloop_ledger::TimelineTransition::Verified,
                rescueloop_ledger::TimelineOutcome::Completed,
                "Original failure replay passed after repair",
                None,
                rescueloop_core::IncidentStatus::VerifiedFixed,
            )
            .await?;
            rescueloop_repair::finalize(&mut transaction, true).await?;
            let receipt = rescueloop_repair::persist(&transaction, &transaction_root).await?;
            report!(
                "VERIFIED: original action now succeeds ({} ms).",
                result.duration_ms
            );
            report!("Transaction receipt: {}", receipt.display());
            record_repair_lineage(
                incident_dir,
                &incident,
                &transaction,
                rescueloop_core::IncidentStatus::VerifiedFixed,
                serde_json::json!({"passed": true, "exit_code": result.exit_code, "duration_ms": result.duration_ms}),
                verbose,
            )
            .await?;
            tracing::info!(
                event = "repair.verified",
                incident_id = %incident.id,
                analysis_id = %analysis_id,
                plan_id = %plan_id,
                repair_transaction_id = %transaction.id,
                verification_id = %verification_id,
                duration_ms = result.duration_ms,
                "Repair verified"
            );
            record_timeline(
                incident_dir,
                &incident,
                timeline_correlation_id,
                repair_ids,
                rescueloop_ledger::TimelineComponent::Ledger,
                rescueloop_ledger::TimelineTransition::Committed,
                rescueloop_ledger::TimelineOutcome::Completed,
                "Verified repair committed with durable lineage",
                None,
                rescueloop_core::IncidentStatus::VerifiedFixed,
            )
            .await?;
        }
        result => {
            let replay_message = match result {
                Ok(value) => format!("exit code {:?}", value.exit_code),
                Err(error) => error.to_string(),
            };
            record_timeline(
                incident_dir,
                &incident,
                timeline_correlation_id,
                repair_ids,
                rescueloop_ledger::TimelineComponent::Verifier,
                rescueloop_ledger::TimelineTransition::Verified,
                rescueloop_ledger::TimelineOutcome::Failed,
                "Original failure replay did not pass after repair",
                Some("objective verification failed or was unavailable"),
                rescueloop_core::IncidentStatus::VerificationFailed,
            )
            .await?;
            if let Err(error) = rescueloop_repair::finalize(&mut transaction, false).await {
                record_timeline(
                    incident_dir,
                    &incident,
                    timeline_correlation_id,
                    repair_ids,
                    rescueloop_ledger::TimelineComponent::Repair,
                    rescueloop_ledger::TimelineTransition::RolledBack,
                    rescueloop_ledger::TimelineOutcome::Failed,
                    "Automatic rollback could not confirm prior-state restoration",
                    Some("rollback operation failed after verification failure"),
                    rescueloop_core::IncidentStatus::VerificationFailed,
                )
                .await?;
                return Err(error).with_context(|| {
                    format!(
                        "CRITICAL: verification failed ({replay_message}) and automatic rollback also failed"
                    )
                });
            }
            crate::metrics::registry().rollback();
            record_timeline(
                incident_dir,
                &incident,
                timeline_correlation_id,
                repair_ids,
                rescueloop_ledger::TimelineComponent::Repair,
                rescueloop_ledger::TimelineTransition::RolledBack,
                rescueloop_ledger::TimelineOutcome::Completed,
                "Original filesystem state restored after failed verification",
                Some("objective verification failed"),
                rescueloop_core::IncidentStatus::RolledBack,
            )
            .await?;
            let receipt = rescueloop_repair::persist(&transaction, &transaction_root).await?;
            report!(
                "ROLLED BACK: verification failed ({replay_message}); original state restored."
            );
            report!("Transaction receipt: {}", receipt.display());
            record_repair_lineage(
                incident_dir,
                &incident,
                &transaction,
                rescueloop_core::IncidentStatus::RolledBack,
                serde_json::json!({"passed": false, "detail": replay_message}),
                verbose,
            )
            .await?;
            tracing::warn!(
                event = "repair.rolled_back",
                incident_id = %incident.id,
                analysis_id = %analysis_id,
                plan_id = %plan_id,
                repair_transaction_id = %transaction.id,
                verification_id = %verification_id,
                reason = replay_message,
                "Repair failed verification and was rolled back"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record_timeline(
    incident_dir: &Path,
    incident: &Incident,
    correlation_id: Uuid,
    ids: crate::timeline::StageIdentifiers,
    component: rescueloop_ledger::TimelineComponent,
    transition: rescueloop_ledger::TimelineTransition,
    outcome: rescueloop_ledger::TimelineOutcome,
    explanation: &str,
    reason: Option<&str>,
    status: rescueloop_core::IncidentStatus,
) -> Result<()> {
    crate::timeline::record_with_ids(
        incident_dir,
        incident,
        crate::timeline::EventSpec {
            correlation_id: Some(correlation_id),
            component,
            transition,
            outcome,
            explanation,
            reason,
            status,
            occurred_at: chrono::Utc::now(),
        },
        ids,
    )
    .await?;
    Ok(())
}

async fn record_repair_lineage(
    incident_dir: &Path,
    incident: &Incident,
    transaction: &rescueloop_repair::Transaction,
    status: rescueloop_core::IncidentStatus,
    verifier: serde_json::Value,
    verbose: bool,
) -> Result<()> {
    let entry = rescueloop_ledger::append(
        &ledger_path(incident_dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: Some(serde_json::to_value(&transaction.action)?),
            before_state: Some(serde_json::json!({"original": transaction.original})),
            after_state: Some(serde_json::json!({"backup": transaction.backup, "transaction_state": transaction.state})),
            verifier: Some(verifier),
            status,
            relation_override: None,
            timeline: None,
        },
    )
    .await?;
    if verbose {
        println!("LINEAGE: {:?}", entry.relation);
    }
    Ok(())
}
