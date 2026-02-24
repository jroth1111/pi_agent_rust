use crate::reliability::state::{FailureClass, PlanRequirement, RuntimeState, TerminalState};
use crate::reliability::task::TaskRuntime;
use chrono::Utc;
use tracing::warn;

#[derive(Debug, Clone)]
pub enum TransitionEvent {
    Dispatch {
        lease_id: String,
        agent_id: String,
        fence_token: u64,
        expires_at: chrono::DateTime<Utc>,
    },
    Submit {
        lease_id: String,
        fence_token: u64,
        patch_digest: String,
        verify_run_id: String,
    },
    VerifySuccess {
        verify_run_id: String,
        patch_digest: String,
    },
    VerifyFail {
        class: FailureClass,
        verify_run_id: Option<String>,
        failure_artifact: Option<String>,
        handoff_summary: String,
    },
    ReportBlocker {
        lease_id: String,
        fence_token: u64,
        reason: String,
        context: String,
    },
    ExpireLease,
    HumanResolve,
    DependenciesMet,
    Supersede {
        by: Vec<String>,
    },
    PromoteRecoverable,
}

#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("invalid transition from {from} on {event}")]
    InvalidTransition { from: String, event: String },
    #[error("fence mismatch: expected {expected}, got {actual}")]
    FenceMismatch { expected: u64, actual: u64 },
    #[error("lease mismatch: expected {expected}, got {actual}")]
    LeaseMismatch { expected: String, actual: String },
    #[error("plan required but not provided: {reason}")]
    PlanRequired { reason: String },
}

/// Optional plan validation data for Ready -> Leased transitions
#[derive(Debug, Clone)]
pub struct PlanGate {
    /// The plan requirement level
    pub requirement: PlanRequirement,
    /// Optional plan validation if a plan was provided
    pub validation: Option<PlanValidationData>,
}

/// Data from a validated plan
#[derive(Debug, Clone)]
pub struct PlanValidationData {
    /// Files the plan touches
    pub touches: Vec<String>,
    /// Test perspectives covered
    pub perspectives: Vec<String>,
    /// Evidence references (if applicable)
    pub evidence_refs: Vec<String>,
}

#[allow(clippy::too_many_lines)]
pub fn apply_transition(
    runtime: &mut TaskRuntime,
    event: &TransitionEvent,
    max_attempts: u8,
) -> Result<(), TransitionError> {
    apply_transition_with_plan_gate(runtime, event, max_attempts, None)
}

/// Apply transition with optional plan validation gate
///
/// The plan gate is checked when transitioning from Ready to Leased state.
/// If a plan is required but not provided, the transition is blocked.
#[allow(clippy::too_many_lines)]
pub fn apply_transition_with_plan_gate(
    runtime: &mut TaskRuntime,
    event: &TransitionEvent,
    max_attempts: u8,
    plan_gate: Option<&PlanGate>,
) -> Result<(), TransitionError> {
    let next_state = match (&runtime.state, event) {
        (RuntimeState::Blocked { .. }, TransitionEvent::DependenciesMet)
        | (RuntimeState::Leased { .. }, TransitionEvent::ExpireLease)
        | (RuntimeState::AwaitingHuman { .. }, TransitionEvent::HumanResolve)
        | (RuntimeState::Recoverable { .. }, TransitionEvent::PromoteRecoverable) => {
            RuntimeState::Ready
        }

        (
            RuntimeState::Ready,
            TransitionEvent::Dispatch {
                lease_id,
                agent_id,
                fence_token,
                expires_at,
            },
        ) => {
            // Check plan requirement gate before allowing execution
            if let Some(gate) = plan_gate {
                check_plan_requirement(gate)?;
            }
            RuntimeState::Leased {
                lease_id: lease_id.clone(),
                agent_id: agent_id.clone(),
                fence_token: *fence_token,
                expires_at: *expires_at,
            }
        }

        (
            RuntimeState::Leased {
                lease_id,
                fence_token,
                ..
            },
            TransitionEvent::Submit {
                lease_id: actual_lease,
                fence_token: actual_fence,
                patch_digest,
                verify_run_id,
            },
        ) => {
            validate_lease_fence(lease_id, *fence_token, actual_lease, *actual_fence)?;
            RuntimeState::Verifying {
                patch_digest: patch_digest.clone(),
                verify_run_id: verify_run_id.clone(),
            }
        }

        (
            RuntimeState::Leased {
                lease_id,
                fence_token,
                ..
            },
            TransitionEvent::ReportBlocker {
                lease_id: actual_lease,
                fence_token: actual_fence,
                reason,
                context,
            },
        ) => {
            validate_lease_fence(lease_id, *fence_token, actual_lease, *actual_fence)?;
            RuntimeState::AwaitingHuman {
                question: reason.clone(),
                context: context.clone(),
                asked_at: Utc::now(),
            }
        }

        (
            RuntimeState::Verifying {
                verify_run_id: expected_run,
                patch_digest: expected_patch,
            },
            TransitionEvent::VerifySuccess {
                verify_run_id,
                patch_digest,
            },
        ) => {
            if expected_run != verify_run_id || expected_patch != patch_digest {
                return Err(TransitionError::InvalidTransition {
                    from: format!("Verifying({expected_run}, {expected_patch})"),
                    event: format!("VerifySuccess({verify_run_id}, {patch_digest})"),
                });
            }
            RuntimeState::Terminal(TerminalState::Succeeded {
                patch_digest: patch_digest.clone(),
                verify_run_id: verify_run_id.clone(),
                completed_at: Utc::now(),
            })
        }

        (
            RuntimeState::Verifying {
                verify_run_id: expected_run,
                ..
            },
            TransitionEvent::VerifyFail {
                class,
                verify_run_id,
                failure_artifact,
                handoff_summary,
            },
        ) => {
            if verify_run_id
                .as_ref()
                .is_some_and(|actual_run_id| actual_run_id != expected_run)
            {
                return Err(TransitionError::InvalidTransition {
                    from: format!("Verifying({expected_run})"),
                    event: format!("VerifyFail({verify_run_id:?})"),
                });
            }

            let run_id = verify_run_id
                .clone()
                .unwrap_or_else(|| expected_run.clone());

            runtime.attempt = runtime.attempt.saturating_add(1);

            if class.needs_human() {
                RuntimeState::AwaitingHuman {
                    question: format!("Verification failed: {class:?}"),
                    context: handoff_summary.clone(),
                    asked_at: Utc::now(),
                }
            } else if class.is_retryable() {
                if runtime.attempt < max_attempts {
                    RuntimeState::Recoverable {
                        reason: *class,
                        failure_artifact: failure_artifact.clone(),
                        handoff_summary: handoff_summary.clone(),
                        retry_after: None,
                    }
                } else {
                    RuntimeState::Terminal(TerminalState::Failed {
                        class: FailureClass::MaxAttemptsExceeded,
                        verify_run_id: Some(run_id),
                        failed_at: Utc::now(),
                    })
                }
            } else {
                RuntimeState::Terminal(TerminalState::Failed {
                    class: *class,
                    verify_run_id: Some(run_id),
                    failed_at: Utc::now(),
                })
            }
        }

        (_, TransitionEvent::Supersede { by }) => {
            RuntimeState::Terminal(TerminalState::Superseded { by: by.clone() })
        }

        (state, e) => {
            return Err(TransitionError::InvalidTransition {
                from: format!("{state:?}"),
                event: format!("{e:?}"),
            });
        }
    };

    runtime.state = next_state;
    runtime.last_transition_at = Utc::now();
    Ok(())
}

fn validate_lease_fence(
    expected_lease_id: &str,
    expected_fence: u64,
    actual_lease_id: &str,
    actual_fence: u64,
) -> Result<(), TransitionError> {
    if expected_lease_id != actual_lease_id {
        return Err(TransitionError::LeaseMismatch {
            expected: expected_lease_id.to_string(),
            actual: actual_lease_id.to_string(),
        });
    }
    if expected_fence != actual_fence {
        return Err(TransitionError::FenceMismatch {
            expected: expected_fence,
            actual: actual_fence,
        });
    }
    Ok(())
}

/// Check if plan requirement is satisfied before allowing Ready -> Leased transition
fn check_plan_requirement(gate: &PlanGate) -> Result<(), TransitionError> {
    match gate.requirement {
        PlanRequirement::None | PlanRequirement::Optional => Ok(()),
        PlanRequirement::Required => {
            if gate.validation.is_none() {
                let msg =
                    "Plan required but not provided. Specify files to touch and test perspectives.";
                warn!("Plan requirement violation: {}", msg);
                return Err(TransitionError::PlanRequired {
                    reason: msg.to_string(),
                });
            }
            // Validate that required fields are present
            let validation = gate.validation.as_ref().unwrap();
            if validation.touches.is_empty() {
                return Err(TransitionError::PlanRequired {
                    reason: "Plan must specify files to touch".to_string(),
                });
            }
            if validation.perspectives.is_empty() {
                return Err(TransitionError::PlanRequired {
                    reason: "Plan must specify test perspectives".to_string(),
                });
            }
            Ok(())
        }
        PlanRequirement::RequiredWithEvidence => {
            if gate.validation.is_none() {
                let msg = "Plan with evidence required but not provided.";
                warn!("Plan requirement violation: {}", msg);
                return Err(TransitionError::PlanRequired {
                    reason: msg.to_string(),
                });
            }
            let validation = gate.validation.as_ref().unwrap();
            if validation.touches.is_empty() {
                return Err(TransitionError::PlanRequired {
                    reason: "Plan must specify files to touch".to_string(),
                });
            }
            if validation.perspectives.is_empty() {
                return Err(TransitionError::PlanRequired {
                    reason: "Plan must specify test perspectives".to_string(),
                });
            }
            if validation.evidence_refs.is_empty() {
                return Err(TransitionError::PlanRequired {
                    reason: "Plan must provide evidence references (file:line format)".to_string(),
                });
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::task::TaskRuntime;
    use chrono::Duration;

    #[test]
    fn transition_happy_path_to_success() {
        let mut runtime = TaskRuntime::new();
        let expires_at = Utc::now() + Duration::hours(1);

        apply_transition(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
        )
        .expect("dispatch");

        apply_transition(
            &mut runtime,
            &TransitionEvent::Submit {
                lease_id: "l1".into(),
                fence_token: 1,
                patch_digest: "sha256:abc".into(),
                verify_run_id: "vr1".into(),
            },
            3,
        )
        .expect("submit");

        apply_transition(
            &mut runtime,
            &TransitionEvent::VerifySuccess {
                verify_run_id: "vr1".into(),
                patch_digest: "sha256:abc".into(),
            },
            3,
        )
        .expect("success");

        assert!(matches!(
            runtime.state,
            RuntimeState::Terminal(TerminalState::Succeeded { .. })
        ));
    }

    #[test]
    fn transition_rejects_fence_mismatch() {
        let mut runtime = TaskRuntime::new();
        let expires_at = Utc::now() + Duration::hours(1);
        apply_transition(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 7,
                expires_at,
            },
            3,
        )
        .expect("dispatch");

        let err = apply_transition(
            &mut runtime,
            &TransitionEvent::Submit {
                lease_id: "l1".into(),
                fence_token: 8,
                patch_digest: "sha256:abc".into(),
                verify_run_id: "vr1".into(),
            },
            3,
        )
        .expect_err("should fail");

        assert!(matches!(err, TransitionError::FenceMismatch { .. }));
    }

    #[test]
    fn transition_rejects_lease_mismatch() {
        let mut runtime = TaskRuntime::new();
        let expires_at = Utc::now() + Duration::hours(1);
        apply_transition(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 7,
                expires_at,
            },
            3,
        )
        .expect("dispatch");

        let err = apply_transition(
            &mut runtime,
            &TransitionEvent::Submit {
                lease_id: "l2".into(),
                fence_token: 7,
                patch_digest: "sha256:abc".into(),
                verify_run_id: "vr1".into(),
            },
            3,
        )
        .expect_err("should fail");

        assert!(matches!(err, TransitionError::LeaseMismatch { .. }));
    }

    #[test]
    fn transition_rejects_verify_success_when_patch_digest_mismatch() {
        let mut runtime = TaskRuntime::new();
        let expires_at = Utc::now() + Duration::hours(1);

        apply_transition(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
        )
        .expect("dispatch");
        apply_transition(
            &mut runtime,
            &TransitionEvent::Submit {
                lease_id: "l1".into(),
                fence_token: 1,
                patch_digest: "sha256:abc".into(),
                verify_run_id: "vr1".into(),
            },
            3,
        )
        .expect("submit");

        let err = apply_transition(
            &mut runtime,
            &TransitionEvent::VerifySuccess {
                verify_run_id: "vr1".into(),
                patch_digest: "sha256:def".into(),
            },
            3,
        )
        .expect_err("must reject digest mismatch");

        assert!(matches!(err, TransitionError::InvalidTransition { .. }));
    }

    #[test]
    fn transition_rejects_verify_fail_when_run_id_mismatch() {
        let mut runtime = TaskRuntime::new();
        let expires_at = Utc::now() + Duration::hours(1);

        apply_transition(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
        )
        .expect("dispatch");
        apply_transition(
            &mut runtime,
            &TransitionEvent::Submit {
                lease_id: "l1".into(),
                fence_token: 1,
                patch_digest: "sha256:abc".into(),
                verify_run_id: "vr1".into(),
            },
            3,
        )
        .expect("submit");

        let err = apply_transition(
            &mut runtime,
            &TransitionEvent::VerifyFail {
                class: FailureClass::VerificationFailed,
                verify_run_id: Some("vr2".into()),
                failure_artifact: None,
                handoff_summary: "summary".into(),
            },
            3,
        )
        .expect_err("must reject run mismatch");

        assert!(matches!(err, TransitionError::InvalidTransition { .. }));
    }

    #[test]
    fn transition_moves_retryable_failure_to_max_attempts_exceeded_terminal() {
        let mut runtime = TaskRuntime::new();
        let expires_at = Utc::now() + Duration::hours(1);

        apply_transition(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            1,
        )
        .expect("dispatch");
        apply_transition(
            &mut runtime,
            &TransitionEvent::Submit {
                lease_id: "l1".into(),
                fence_token: 1,
                patch_digest: "sha256:abc".into(),
                verify_run_id: "vr1".into(),
            },
            1,
        )
        .expect("submit");

        apply_transition(
            &mut runtime,
            &TransitionEvent::VerifyFail {
                class: FailureClass::VerificationFailed,
                verify_run_id: Some("vr1".into()),
                failure_artifact: None,
                handoff_summary: "summary".into(),
            },
            1,
        )
        .expect("verify fail");

        assert!(matches!(
            runtime.state,
            RuntimeState::Terminal(TerminalState::Failed {
                class: FailureClass::MaxAttemptsExceeded,
                ..
            })
        ));
    }

    #[test]
    fn transition_rejects_events_after_terminal_state() {
        let mut runtime = TaskRuntime::new();
        runtime.state = RuntimeState::Terminal(TerminalState::Canceled {
            reason: "done".into(),
            canceled_at: Utc::now(),
        });

        let err = apply_transition(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at: Utc::now() + Duration::minutes(30),
            },
            3,
        )
        .expect_err("terminal state should be immutable");
        assert!(matches!(err, TransitionError::InvalidTransition { .. }));
    }

    // Plan gate tests
    #[test]
    fn plan_gate_allows_dispatch_when_none_required() {
        let mut runtime = TaskRuntime::new();
        let gate = PlanGate {
            requirement: PlanRequirement::None,
            validation: None,
        };
        let expires_at = Utc::now() + Duration::hours(1);

        let result = apply_transition_with_plan_gate(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
            Some(&gate),
        );
        assert!(result.is_ok());
        assert!(matches!(runtime.state, RuntimeState::Leased { .. }));
    }

    #[test]
    fn plan_gate_allows_dispatch_when_optional_with_no_plan() {
        let mut runtime = TaskRuntime::new();
        let gate = PlanGate {
            requirement: PlanRequirement::Optional,
            validation: None,
        };
        let expires_at = Utc::now() + Duration::hours(1);

        let result = apply_transition_with_plan_gate(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
            Some(&gate),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn plan_gate_blocks_dispatch_when_required_no_plan() {
        let mut runtime = TaskRuntime::new();
        let gate = PlanGate {
            requirement: PlanRequirement::Required,
            validation: None,
        };
        let expires_at = Utc::now() + Duration::hours(1);

        let err = apply_transition_with_plan_gate(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
            Some(&gate),
        )
        .expect_err("should block when plan required but not provided");

        assert!(matches!(err, TransitionError::PlanRequired { .. }));
        assert!(matches!(runtime.state, RuntimeState::Ready));
    }

    #[test]
    fn plan_gate_allows_dispatch_when_required_with_valid_plan() {
        let mut runtime = TaskRuntime::new();
        let validation = PlanValidationData {
            touches: vec!["src/main.rs".to_string()],
            perspectives: vec!["HappyPath".to_string()],
            evidence_refs: vec![],
        };
        let gate = PlanGate {
            requirement: PlanRequirement::Required,
            validation: Some(validation),
        };
        let expires_at = Utc::now() + Duration::hours(1);

        let result = apply_transition_with_plan_gate(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
            Some(&gate),
        );
        assert!(result.is_ok());
        assert!(matches!(runtime.state, RuntimeState::Leased { .. }));
    }

    #[test]
    fn plan_gate_blocks_dispatch_when_required_but_empty_touches() {
        let mut runtime = TaskRuntime::new();
        let validation = PlanValidationData {
            touches: vec![],
            perspectives: vec!["HappyPath".to_string()],
            evidence_refs: vec![],
        };
        let gate = PlanGate {
            requirement: PlanRequirement::Required,
            validation: Some(validation),
        };
        let expires_at = Utc::now() + Duration::hours(1);

        let err = apply_transition_with_plan_gate(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
            Some(&gate),
        )
        .expect_err("should block when plan has no touches");

        assert!(matches!(err, TransitionError::PlanRequired { .. }));
    }

    #[test]
    fn plan_gate_blocks_dispatch_when_required_with_evidence_no_evidence() {
        let mut runtime = TaskRuntime::new();
        let validation = PlanValidationData {
            touches: vec!["src/main.rs".to_string()],
            perspectives: vec!["HappyPath".to_string()],
            evidence_refs: vec![],
        };
        let gate = PlanGate {
            requirement: PlanRequirement::RequiredWithEvidence,
            validation: Some(validation),
        };
        let expires_at = Utc::now() + Duration::hours(1);

        let err = apply_transition_with_plan_gate(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
            Some(&gate),
        )
        .expect_err("should block when evidence required but not provided");

        assert!(matches!(err, TransitionError::PlanRequired { .. }));
    }

    #[test]
    fn plan_gate_allows_dispatch_when_required_with_evidence_and_valid() {
        let mut runtime = TaskRuntime::new();
        let validation = PlanValidationData {
            touches: vec!["src/main.rs".to_string()],
            perspectives: vec!["HappyPath".to_string()],
            evidence_refs: vec!["src/main.rs:42".to_string()],
        };
        let gate = PlanGate {
            requirement: PlanRequirement::RequiredWithEvidence,
            validation: Some(validation),
        };
        let expires_at = Utc::now() + Duration::hours(1);

        let result = apply_transition_with_plan_gate(
            &mut runtime,
            &TransitionEvent::Dispatch {
                lease_id: "l1".into(),
                agent_id: "a1".into(),
                fence_token: 1,
                expires_at,
            },
            3,
            Some(&gate),
        );
        assert!(result.is_ok());
        assert!(matches!(runtime.state, RuntimeState::Leased { .. }));
    }
}
