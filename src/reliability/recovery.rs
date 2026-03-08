use crate::reliability::state::{FailureClass, RuntimeState};
use crate::reliability::state_machine::{TransitionEvent, apply_transition};
use crate::reliability::task::TaskNode;
use chrono::{Duration, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    Promoted(String),
    Deferred(String),
    AwaitingHuman(String),
}

#[derive(Debug, Default)]
pub struct RecoveryManager;

impl RecoveryManager {
    pub fn handle_verify_failure(
        task: &mut TaskNode,
        class: FailureClass,
        verify_run_id: Option<String>,
        failure_artifact: Option<String>,
        handoff_summary: String,
    ) -> Result<RecoveryAction, crate::reliability::TransitionError> {
        let event = TransitionEvent::VerifyFail {
            class,
            verify_run_id,
            failure_artifact,
            handoff_summary,
        };
        apply_transition(&mut task.runtime, &event, task.spec.max_attempts)?;

        let action = match task.runtime.state {
            RuntimeState::AwaitingHuman { .. } => RecoveryAction::AwaitingHuman(task.id.clone()),
            _ => RecoveryAction::Deferred(task.id.clone()),
        };
        Ok(action)
    }

    pub fn set_retry_after(task: &mut TaskNode, after_seconds: i64) {
        if let RuntimeState::Recoverable {
            ref mut retry_after,
            ..
        } = task.runtime.state
        {
            *retry_after = Some(Utc::now() + Duration::seconds(after_seconds.max(1)));
        }
    }

    pub fn promote_recoverable(tasks: &mut [TaskNode]) -> Vec<RecoveryAction> {
        let now = Utc::now();
        let mut promoted = Vec::new();

        for task in tasks.iter_mut() {
            let should_promote = match &task.runtime.state {
                RuntimeState::Recoverable { retry_after, .. } => {
                    retry_after.is_none_or(|ra| ra <= now)
                }
                _ => false,
            };
            if should_promote
                && apply_transition(
                    &mut task.runtime,
                    &TransitionEvent::PromoteRecoverable,
                    task.spec.max_attempts,
                )
                .is_ok()
            {
                promoted.push(RecoveryAction::Promoted(task.id.clone()));
            }
        }

        promoted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::state::RuntimeState;
    use crate::reliability::task::{TaskConstraintSet, TaskRuntime, TaskSpec, VerifyPlan};

    fn task_node(id: &str, max_attempts: u8) -> TaskNode {
        TaskNode {
            id: id.to_string(),
            spec: TaskSpec {
                objective: "objective".to_string(),
                constraints: TaskConstraintSet::default(),
                verify: VerifyPlan::Standard {
                    audit_diff: true,
                    command: "echo ok".to_string(),
                    timeout_sec: 10,
                },
                max_attempts,
                input_snapshot: "abc123".to_string(),
                acceptance_ids: vec!["ac-1".to_string()],
                planned_touches: vec!["src/reliability/recovery.rs".to_string()],
            },
            runtime: TaskRuntime::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn recovery_promotes_recoverable_when_due() {
        let mut task = task_node("t1", 3);
        task.runtime.state = RuntimeState::Recoverable {
            reason: FailureClass::VerificationFailed,
            failure_artifact: None,
            handoff_summary: "summary".to_string(),
            retry_after: None,
        };

        let out = RecoveryManager::promote_recoverable(std::slice::from_mut(&mut task));
        assert_eq!(out, vec![RecoveryAction::Promoted("t1".to_string())]);
        assert!(matches!(task.runtime.state, RuntimeState::Ready));
    }

    #[test]
    fn recovery_respects_retry_after_future() {
        let mut task = task_node("t1", 3);
        task.runtime.state = RuntimeState::Recoverable {
            reason: FailureClass::InfraTransient,
            failure_artifact: None,
            handoff_summary: "summary".to_string(),
            retry_after: Some(Utc::now() + Duration::hours(1)),
        };

        let out = RecoveryManager::promote_recoverable(std::slice::from_mut(&mut task));
        assert!(out.is_empty());
        assert!(matches!(
            task.runtime.state,
            RuntimeState::Recoverable { .. }
        ));
    }
}
