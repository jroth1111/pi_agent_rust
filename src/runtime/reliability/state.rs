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
    Until {
        until: DateTime<Utc>,
    },
    DependsOn {
        #[serde(rename = "taskId")]
        task_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defer_trigger_until_serialization_roundtrip() {
        let trigger = DeferTrigger::Until { until: Utc::now() };
        let json = serde_json::to_value(&trigger).unwrap();
        assert_eq!(json["trigger"], "until");
        assert!(json["until"].is_string());

        let back: DeferTrigger = serde_json::from_value(json).unwrap();
        assert_eq!(trigger, back);
    }

    #[test]
    fn defer_trigger_depends_on_serialization_roundtrip() {
        let trigger = DeferTrigger::DependsOn {
            task_id: "task-a".to_string(),
        };
        let json = serde_json::to_value(&trigger).unwrap();
        assert_eq!(json["trigger"], "depends_on");
        assert_eq!(json["taskId"], "task-a");

        let back: DeferTrigger = serde_json::from_value(json).unwrap();
        assert_eq!(trigger, back);
    }
}
