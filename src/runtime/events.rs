use crate::runtime::types::{
    ApprovalCheckpoint, ApprovalId, ApprovalState, ContinuationReason, PlanArtifact, RunId,
    RunPhase, TaskId, TaskNode, TaskState,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEventKind {
    RunCreated,
    PlanningStarted,
    PlanMaterialized {
        plan: PlanArtifact,
        tasks: Vec<TaskNode>,
    },
    PlanAccepted,
    PhaseChanged {
        phase: RunPhase,
        summary: Option<String>,
    },
    TaskStateChanged {
        task_id: TaskId,
        state: TaskState,
        reason: Option<String>,
        retry_at: Option<DateTime<Utc>>,
        continuation_reason: Option<ContinuationReason>,
    },
    ApprovalRequested {
        checkpoint: ApprovalCheckpoint,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        state: ApprovalState,
    },
    WakeScheduled {
        wake_at: DateTime<Utc>,
        reason: String,
    },
    RunCompleted,
    RunFailed {
        reason: String,
    },
    RunCanceled {
        reason: String,
    },
}

impl RuntimeEventKind {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::RunCreated => "run_created",
            Self::PlanningStarted => "planning_started",
            Self::PlanMaterialized { .. } => "plan_materialized",
            Self::PlanAccepted => "plan_accepted",
            Self::PhaseChanged { .. } => "phase_changed",
            Self::TaskStateChanged { .. } => "task_state_changed",
            Self::ApprovalRequested { .. } => "approval_requested",
            Self::ApprovalResolved { .. } => "approval_resolved",
            Self::WakeScheduled { .. } => "wake_scheduled",
            Self::RunCompleted => "run_completed",
            Self::RunFailed { .. } => "run_failed",
            Self::RunCanceled { .. } => "run_canceled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeEvent {
    pub event_id: String,
    pub run_id: RunId,
    pub happened_at: DateTime<Utc>,
    pub kind: RuntimeEventKind,
}

impl RuntimeEvent {
    pub fn new(run_id: impl Into<RunId>, kind: RuntimeEventKind) -> Self {
        Self {
            event_id: format!("evt-{}", uuid::Uuid::new_v4().simple()),
            run_id: run_id.into(),
            happened_at: Utc::now(),
            kind,
        }
    }

    pub const fn label(&self) -> &'static str {
        self.kind.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::types::{RunBudgets, RunConstraints, RunSpec};
    use std::path::PathBuf;

    fn sample_plan() -> PlanArtifact {
        PlanArtifact {
            plan_id: "plan-1".to_string(),
            digest: "digest".to_string(),
            objective: "test".to_string(),
            task_drafts: vec!["task-a".to_string()],
            touched_paths: Vec::new(),
            test_strategy: vec!["cargo test".to_string()],
            evidence_refs: Vec::new(),
            produced_at: Utc::now(),
        }
    }

    fn sample_task() -> TaskNode {
        TaskNode::new(crate::runtime::types::TaskSpec {
            task_id: "task-a".to_string(),
            title: "task".to_string(),
            objective: "ship it".to_string(),
            planned_touches: Vec::new(),
            verify: crate::runtime::types::VerifySpec {
                command: "cargo test".to_string(),
                timeout_sec: 60,
                acceptance_ids: Vec::new(),
            },
            autonomy: crate::runtime::types::AutonomyLevel::Guarded,
            constraints: crate::runtime::types::TaskConstraints::default(),
        })
    }

    #[test]
    fn event_label_matches_kind() {
        let event = RuntimeEvent::new(
            "run-1",
            RuntimeEventKind::PlanMaterialized {
                plan: sample_plan(),
                tasks: vec![sample_task()],
            },
        );
        assert_eq!(event.label(), "plan_materialized");
        assert!(event.event_id.starts_with("evt-"));
    }

    #[test]
    fn phase_changed_label_is_stable() {
        let event = RuntimeEvent::new(
            "run-1",
            RuntimeEventKind::PhaseChanged {
                phase: RunPhase::Running,
                summary: Some("started".to_string()),
            },
        );
        assert_eq!(event.label(), "phase_changed");
    }

    #[test]
    fn plan_artifact_round_trip_shape_stays_serializable() {
        let spec = RunSpec {
            run_id: "run-1".to_string(),
            objective: "obj".to_string(),
            root_workspace: PathBuf::from("/tmp"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            run_verify_command: Some("cargo test".to_string()),
            run_verify_timeout_sec: Some(60),
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        };
        let payload = serde_json::to_value(RuntimeEvent::new(
            spec.run_id,
            RuntimeEventKind::PlanMaterialized {
                plan: sample_plan(),
                tasks: vec![sample_task()],
            },
        ))
        .expect("serialize");
        assert_eq!(payload["kind"]["kind"], "plan_materialized");
    }
}
