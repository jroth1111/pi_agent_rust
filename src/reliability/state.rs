use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Runtime lifecycle for a reliability-tracked task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeState {
    Blocked {
        waiting_on: Vec<String>,
    },
    Ready,
    Leased {
        lease_id: String,
        agent_id: String,
        fence_token: u64,
        expires_at: DateTime<Utc>,
    },
    Verifying {
        patch_digest: String,
        verify_run_id: String,
    },
    Recoverable {
        reason: FailureClass,
        failure_artifact: Option<String>,
        handoff_summary: String,
        retry_after: Option<DateTime<Utc>>,
    },
    AwaitingHuman {
        question: String,
        context: String,
        asked_at: DateTime<Utc>,
    },
    Terminal(TerminalState),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TerminalState {
    Succeeded {
        patch_digest: String,
        verify_run_id: String,
        completed_at: DateTime<Utc>,
    },
    Failed {
        class: FailureClass,
        verify_run_id: Option<String>,
        failed_at: DateTime<Utc>,
    },
    Superseded {
        by: Vec<String>,
    },
    Canceled {
        reason: String,
        canceled_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    VerificationFailed,
    ScopeCreepDetected,
    MergeConflict,
    InfraTransient,
    InfraPermanent,
    HumanBlocker,
    MaxAttemptsExceeded,
}

impl FailureClass {
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::InfraTransient
                | Self::VerificationFailed
                | Self::MergeConflict
                | Self::ScopeCreepDetected
        )
    }

    pub const fn needs_human(self) -> bool {
        matches!(self, Self::HumanBlocker | Self::InfraPermanent)
    }
}

/// Defer semantics used by liveness policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "trigger", rename_all = "snake_case")]
pub enum DeferTrigger {
    Until(DateTime<Utc>),
    DependsOn(String),
}

/// Requirements for planning before execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanRequirement {
    /// No plan needed (single-step tasks)
    #[default]
    None,
    /// Plan suggested but not enforced
    Optional,
    /// Plan required before execution
    Required,
    /// Plan required with evidence references
    RequiredWithEvidence,
}

impl PlanRequirement {
    /// Determine requirement based on task characteristics
    pub fn infer(files_to_touch: usize, has_dependencies: bool, complexity: f32) -> Self {
        // Dependencies always require planning
        if has_dependencies {
            return Self::Required;
        }

        // High complexity always requires planning
        if complexity >= 0.6 {
            return Self::Required;
        }

        // Many files require planning
        if files_to_touch > 3 {
            return Self::Required;
        }

        // Simple single-file task with low complexity
        if files_to_touch <= 1 && complexity < 0.3 {
            Self::None
        } else if files_to_touch <= 3 && complexity < 0.6 {
            Self::Optional
        } else {
            Self::Required
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_requirement_infer_simple_task() {
        // Simple: 1 file, no deps, low complexity
        let req = PlanRequirement::infer(1, false, 0.2);
        assert_eq!(req, PlanRequirement::None);
    }

    #[test]
    fn plan_requirement_infer_moderate_task() {
        // Moderate: 2-3 files, low-mid complexity
        let req = PlanRequirement::infer(2, false, 0.5);
        assert_eq!(req, PlanRequirement::Optional);
    }

    #[test]
    fn plan_requirement_infer_complex_task() {
        // Complex: many files or high complexity
        let req = PlanRequirement::infer(5, false, 0.7);
        assert_eq!(req, PlanRequirement::Required);

        let req = PlanRequirement::infer(2, true, 0.5);
        assert_eq!(req, PlanRequirement::Required);
    }

    #[test]
    fn plan_requirement_infer_with_dependencies() {
        // Even with few files, dependencies push to Required
        let req = PlanRequirement::infer(1, true, 0.2);
        assert_eq!(req, PlanRequirement::Required);
    }

    #[test]
    fn plan_requirement_serialization_roundtrip() {
        let req = PlanRequirement::RequiredWithEvidence;
        let json = serde_json::to_string(&req).unwrap();
        let deser: PlanRequirement = serde_json::from_str(&json).unwrap();
        assert_eq!(req, deser);
    }
}
