use anyhow::{Context, Result, bail};
use rescueloop_core::{AnalysisId, PlanId, ProposedAction, RepairTransactionId, VerificationId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationalAction {
    RestartContainer {
        engine: String,
        container_id: String,
    },
    RestartService {
        service_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationalReceipt {
    pub id: RepairTransactionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_id: Option<AnalysisId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<PlanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_id: Option<VerificationId>,
    pub action: OperationalAction,
    pub previous_running: bool,
    pub verified: bool,
    pub rolled_back: bool,
}

pub fn compile_operational(proposal: &ProposedAction) -> Result<Option<OperationalAction>> {
    let string = |key: &str| {
        proposal
            .parameters
            .get(key)
            .and_then(|value| value.as_str())
            .with_context(|| format!("{} has no {key}", proposal.action_type))
    };
    let action = match proposal.action_type.as_str() {
        "restart_container" => {
            let engine = string("engine")?.to_string();
            if !matches!(engine.as_str(), "docker" | "podman") {
                bail!("unsupported container engine")
            }
            OperationalAction::RestartContainer {
                engine,
                container_id: string("container_id")?.to_string(),
            }
        }
        "restart_service" => OperationalAction::RestartService {
            service_id: string("service_id")?.to_string(),
        },
        _ => return Ok(None),
    };
    Ok(Some(action))
}

#[tracing::instrument(
    name = "repair.operational",
    skip(action, allowed_id),
    fields(
        analysis_id = %analysis_id,
        plan_id = %plan_id,
        repair_transaction_id = %repair_transaction_id,
        verification_id = %verification_id
    ),
    err
)]
pub async fn execute_operational(
    action: OperationalAction,
    allowed_id: &str,
    analysis_id: AnalysisId,
    plan_id: PlanId,
    repair_transaction_id: RepairTransactionId,
    verification_id: VerificationId,
) -> Result<OperationalReceipt> {
    let id = match &action {
        OperationalAction::RestartContainer { container_id, .. } => container_id,
        OperationalAction::RestartService { service_id } => service_id,
    };
    if id != allowed_id || id.is_empty() {
        bail!("operational target is not the exact evidenced identity")
    }
    match &action {
        OperationalAction::RestartContainer {
            engine,
            container_id,
        } => {
            let previous_running = container_running(engine, container_id).await?;
            let status = tokio::process::Command::new(engine)
                .args(["restart", container_id])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await?;
            if !status.success() {
                bail!("container engine rejected restart")
            }
            let verified = container_running(engine, container_id).await?;
            let mut rolled_back = false;
            if !verified && !previous_running {
                let _ = tokio::process::Command::new(engine)
                    .args(["stop", container_id])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
                rolled_back = true;
            }
            Ok(OperationalReceipt {
                id: repair_transaction_id,
                analysis_id: Some(analysis_id),
                plan_id: Some(plan_id),
                verification_id: Some(verification_id),
                action,
                previous_running,
                verified,
                rolled_back,
            })
        }
        OperationalAction::RestartService { service_id } => {
            execute_service(
                action.clone(),
                service_id,
                analysis_id,
                plan_id,
                repair_transaction_id,
                verification_id,
            )
            .await
        }
    }
}

async fn container_running(engine: &str, id: &str) -> Result<bool> {
    let output = tokio::process::Command::new(engine)
        .args(["inspect", "--format", "{{.State.Running}}", id])
        .output()
        .await?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

#[allow(clippy::needless_return)]
async fn execute_service(
    action: OperationalAction,
    service_id: &str,
    analysis_id: AnalysisId,
    plan_id: PlanId,
    repair_transaction_id: RepairTransactionId,
    verification_id: VerificationId,
) -> Result<OperationalReceipt> {
    #[cfg(target_os = "macos")]
    {
        let previous_running = tokio::process::Command::new("launchctl")
            .args(["print", service_id])
            .output()
            .await?
            .status
            .success();
        let status = tokio::process::Command::new("launchctl")
            .args(["kickstart", "-k", service_id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        let verified = status.success()
            && tokio::process::Command::new("launchctl")
                .args(["print", service_id])
                .output()
                .await?
                .status
                .success();
        return Ok(OperationalReceipt {
            id: repair_transaction_id,
            analysis_id: Some(analysis_id),
            plan_id: Some(plan_id),
            verification_id: Some(verification_id),
            action,
            previous_running,
            verified,
            rolled_back: false,
        });
    }
    #[cfg(target_os = "windows")]
    {
        let previous_running = tokio::process::Command::new("sc.exe")
            .args(["query", service_id])
            .output()
            .await?
            .stdout
            .windows(7)
            .any(|value| value == b"RUNNING");
        let _ = tokio::process::Command::new("sc.exe")
            .args(["stop", service_id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        let status = tokio::process::Command::new("sc.exe")
            .args(["start", service_id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        return Ok(OperationalReceipt {
            id: repair_transaction_id,
            analysis_id: Some(analysis_id),
            plan_id: Some(plan_id),
            verification_id: Some(verification_id),
            action,
            previous_running,
            verified: status.success(),
            rolled_back: false,
        });
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("service repair supports macOS and Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_receipts_preserve_ids_and_accept_legacy_documents() {
        let receipt = OperationalReceipt {
            id: RepairTransactionId::new(),
            analysis_id: Some(AnalysisId::new()),
            plan_id: Some(PlanId::new()),
            verification_id: Some(VerificationId::new()),
            action: OperationalAction::RestartService {
                service_id: "fixture.service".into(),
            },
            previous_running: true,
            verified: true,
            rolled_back: false,
        };
        let encoded = serde_json::to_value(&receipt).unwrap();
        let decoded: OperationalReceipt = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.analysis_id, receipt.analysis_id);
        assert_eq!(decoded.plan_id, receipt.plan_id);
        assert_eq!(decoded.verification_id, receipt.verification_id);

        let mut legacy = encoded;
        let object = legacy.as_object_mut().unwrap();
        object.remove("analysis_id");
        object.remove("plan_id");
        object.remove("verification_id");
        let decoded: OperationalReceipt = serde_json::from_value(legacy).unwrap();
        assert!(decoded.analysis_id.is_none());
        assert!(decoded.plan_id.is_none());
        assert!(decoded.verification_id.is_none());
    }
}
