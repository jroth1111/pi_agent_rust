use crate::runtime::events::{RuntimeEvent, RuntimeEventKind};
use crate::runtime::types::{
    ApprovalState, ContinuationReason, FailureRecord, RunPhase, RunSnapshot, TaskId, TaskNode,
    TaskState,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum TransitionError {
    #[error("run event {event} does not match snapshot run id {run_id}")]
    RunIdMismatch { run_id: String, event: String },
    #[error("task not found: {0}")]
    UnknownTask(TaskId),
    #[error("approval not found: {0}")]
    UnknownApproval(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeTransition {
    pub event_id: String,
    pub event_label: String,
    pub from_phase: RunPhase,
    pub to_phase: RunPhase,
}

#[derive(Debug, Clone)]
pub struct RuntimeStateMachine {
    snapshot: RunSnapshot,
    history: Vec<RuntimeTransition>,
}

impl RuntimeStateMachine {
    pub fn new(snapshot: RunSnapshot) -> Self {
        Self {
            snapshot,
            history: Vec::new(),
        }
    }

    pub const fn snapshot(&self) -> &RunSnapshot {
        &self.snapshot
    }

    pub fn into_snapshot(self) -> RunSnapshot {
        self.snapshot
    }

    pub fn history(&self) -> &[RuntimeTransition] {
        &self.history
    }

    pub fn apply(&mut self, event: RuntimeEvent) -> Result<&RuntimeTransition, TransitionError> {
        if event.run_id != self.snapshot.spec.run_id {
            return Err(TransitionError::RunIdMismatch {
                run_id: self.snapshot.spec.run_id.clone(),
                event: event.run_id,
            });
        }

        let event_id = event.event_id.clone();
        let event_label = event.label().to_string();
        let from_phase = self.snapshot.phase;
        match event.kind {
            RuntimeEventKind::RunCreated => {
                self.snapshot.phase = RunPhase::Created;
                self.snapshot.summary.next_action = Some("start planning".to_string());
            }
            RuntimeEventKind::PlanningStarted => {
                self.snapshot.phase = RunPhase::Planning;
                self.snapshot.summary.next_action = Some("materialize plan".to_string());
            }
            RuntimeEventKind::PlanAccepted { plan, tasks } => {
                self.snapshot.phase = RunPhase::Dispatching;
                self.snapshot.plan = Some(plan);
                self.install_tasks(tasks);
                self.snapshot.summary.next_action = Some("dispatch ready tasks".to_string());
            }
            RuntimeEventKind::PhaseChanged { phase, summary } => {
                self.snapshot.phase = phase;
                self.snapshot.summary.next_action = summary;
            }
            RuntimeEventKind::TaskStateChanged {
                task_id,
                state,
                reason,
                retry_at,
                continuation_reason,
            } => {
                let task = self
                    .snapshot
                    .tasks
                    .get_mut(&task_id)
                    .ok_or_else(|| TransitionError::UnknownTask(task_id.clone()))?;
                task.runtime.state = state;
                task.runtime.retry_at = retry_at;
                task.runtime.continuation_reason = continuation_reason;
                task.runtime.last_error = reason.map(|message| FailureRecord {
                    code: state_error_code(state).to_string(),
                    message,
                    retry_at,
                });
                if matches!(state, TaskState::AwaitingHuman) {
                    self.snapshot.phase = RunPhase::AwaitingHuman;
                } else if matches!(state, TaskState::Recoverable) {
                    self.snapshot.phase = RunPhase::Recovering;
                }
                self.rebuild_ready_queue();
                self.snapshot.summary.next_action =
                    next_action_for_phase(self.snapshot.phase, continuation_reason);
            }
            RuntimeEventKind::ApprovalRequested { checkpoint } => {
                self.snapshot.phase = RunPhase::AwaitingHuman;
                self.snapshot
                    .approvals
                    .insert(checkpoint.approval_id.clone(), checkpoint);
                self.snapshot.summary.next_action = Some("await human approval".to_string());
            }
            RuntimeEventKind::ApprovalResolved { approval_id, state } => {
                let approval = self
                    .snapshot
                    .approvals
                    .get_mut(&approval_id)
                    .ok_or_else(|| TransitionError::UnknownApproval(approval_id.clone()))?;
                approval.state = state;
                self.snapshot.phase = if matches!(state, ApprovalState::Approved) {
                    RunPhase::Recovering
                } else {
                    RunPhase::AwaitingHuman
                };
                self.snapshot.summary.next_action = if matches!(state, ApprovalState::Approved) {
                    Some("resume recovered work".to_string())
                } else {
                    Some("human approval denied".to_string())
                };
            }
            RuntimeEventKind::WakeScheduled { wake_at, reason } => {
                self.snapshot.wake_at = Some(wake_at);
                self.snapshot.summary.next_action = Some(reason);
            }
            RuntimeEventKind::RunCompleted => {
                self.snapshot.phase = RunPhase::Completed;
                self.snapshot.summary.next_action = None;
            }
            RuntimeEventKind::RunFailed { reason } => {
                self.snapshot.phase = RunPhase::Failed;
                self.snapshot.summary.blockers.push(reason);
                self.snapshot.summary.next_action = None;
            }
            RuntimeEventKind::RunCanceled { reason } => {
                self.snapshot.phase = RunPhase::Canceled;
                self.snapshot.summary.blockers.push(reason);
                self.snapshot.summary.next_action = None;
            }
        }

        self.snapshot.version = self.snapshot.version.saturating_add(1);
        self.snapshot.updated_at = Utc::now();
        self.refresh_task_counts();
        let to_phase = self.snapshot.phase;
        self.history.push(RuntimeTransition {
            event_id,
            event_label,
            from_phase,
            to_phase,
        });
        Ok(self.history.last().expect("transition should exist"))
    }

    fn install_tasks(&mut self, tasks: Vec<TaskNode>) {
        self.snapshot.tasks.clear();
        for mut task in tasks {
            if task.runtime.state == TaskState::Draft {
                task.runtime.state = if task.deps.is_empty() {
                    TaskState::Ready
                } else {
                    TaskState::Blocked
                };
            }
            self.snapshot.tasks.insert(task.spec.task_id.clone(), task);
        }
        self.rebuild_ready_queue();
    }

    fn rebuild_ready_queue(&mut self) {
        self.snapshot.ready_queue.clear();
        for (task_id, task) in &self.snapshot.tasks {
            if task.runtime.state == TaskState::Ready {
                self.snapshot.ready_queue.push_back(task_id.clone());
            }
        }
    }

    fn refresh_task_counts(&mut self) {
        self.snapshot.summary.task_counts.clear();
        for task in self.snapshot.tasks.values() {
            let label = format!("{:?}", task.runtime.state).to_ascii_lowercase();
            *self.snapshot.summary.task_counts.entry(label).or_insert(0) += 1;
        }
    }
}

fn state_error_code(state: TaskState) -> &'static str {
    match state {
        TaskState::Recoverable => "recoverable",
        TaskState::AwaitingHuman => "awaiting_human",
        TaskState::Failed => "failed",
        _ => "state_change",
    }
}

fn next_action_for_phase(
    phase: RunPhase,
    continuation_reason: Option<ContinuationReason>,
) -> Option<String> {
    if phase.is_terminal() {
        return None;
    }
    Some(
        match continuation_reason.unwrap_or(ContinuationReason::TaskReady) {
            ContinuationReason::PlanExecution => "execute planned tasks".to_string(),
            ContinuationReason::TaskReady => match phase {
                RunPhase::Dispatching => "dispatch ready tasks".to_string(),
                RunPhase::Running => "continue active work".to_string(),
                RunPhase::Verifying => "verify current outputs".to_string(),
                RunPhase::Recovering => "retry recoverable work".to_string(),
                RunPhase::AwaitingHuman => "await human input".to_string(),
                RunPhase::Planning => "materialize plan".to_string(),
                RunPhase::Created => "start planning".to_string(),
                RunPhase::Completed | RunPhase::Failed | RunPhase::Canceled => {
                    "no further action".to_string()
                }
            },
            ContinuationReason::VerificationRetry => "retry failed verification".to_string(),
            ContinuationReason::BackgroundTaskCompletion => {
                "consume background results".to_string()
            }
            ContinuationReason::ApprovalResolved => "resume approved work".to_string(),
            ContinuationReason::StuckReflection => "reflect and reschedule".to_string(),
            ContinuationReason::WakeTimer => "resume scheduled work".to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::events::RuntimeEventKind;
    use crate::runtime::types::{
        ApprovalCheckpoint, ApprovalState, AutonomyLevel, RunBudgets, RunConstraints, RunSpec,
        TaskConstraints, TaskSpec, VerifySpec,
    };
    use std::path::PathBuf;

    fn sample_snapshot() -> RunSnapshot {
        RunSnapshot::new(RunSpec {
            run_id: "run-1".to_string(),
            objective: "ship runtime".to_string(),
            root_workspace: PathBuf::from("/tmp/pi"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            run_verify_command: Some("cargo test".to_string()),
            run_verify_timeout_sec: Some(60),
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        })
    }

    fn sample_task(task_id: &str) -> TaskNode {
        TaskNode::new(TaskSpec {
            task_id: task_id.to_string(),
            title: task_id.to_string(),
            objective: "do work".to_string(),
            planned_touches: Vec::new(),
            verify: VerifySpec {
                command: "cargo test".to_string(),
                timeout_sec: 60,
                acceptance_ids: Vec::new(),
            },
            autonomy: AutonomyLevel::Guarded,
            constraints: TaskConstraints::default(),
        })
    }

    #[test]
    fn plan_acceptance_materializes_ready_tasks() {
        let mut machine = RuntimeStateMachine::new(sample_snapshot());
        let event = RuntimeEvent::new(
            "run-1",
            RuntimeEventKind::PlanAccepted {
                plan: crate::runtime::types::PlanArtifact {
                    plan_id: "plan-1".to_string(),
                    digest: "digest".to_string(),
                    objective: "ship".to_string(),
                    task_drafts: vec!["task-1".to_string()],
                    touched_paths: Vec::new(),
                    test_strategy: vec!["cargo test".to_string()],
                    evidence_refs: Vec::new(),
                    produced_at: Utc::now(),
                },
                tasks: vec![sample_task("task-1")],
            },
        );
        machine.apply(event).expect("apply");
        assert_eq!(machine.snapshot().phase, RunPhase::Dispatching);
        assert_eq!(machine.snapshot().ready_queue.len(), 1);
    }

    #[test]
    fn approval_request_enters_awaiting_human() {
        let mut machine = RuntimeStateMachine::new(sample_snapshot());
        let checkpoint = ApprovalCheckpoint {
            approval_id: "appr-1".to_string(),
            reason: "needs approval".to_string(),
            context: "high risk".to_string(),
            state: ApprovalState::Pending,
            created_at: Utc::now(),
        };
        machine
            .apply(RuntimeEvent::new(
                "run-1",
                RuntimeEventKind::ApprovalRequested { checkpoint },
            ))
            .expect("apply");
        assert_eq!(machine.snapshot().phase, RunPhase::AwaitingHuman);
        assert_eq!(machine.snapshot().approvals.len(), 1);
    }
}
