//! Agent-local audit state for closeout gates, evidence, and discovery.
//!
//! This module keeps session-local artifacts that the agent still uses
//! directly: immutable task event logs, completion gates, and discovery
//! tracking. It no longer owns the runtime lifecycle; orchestration state
//! lives under `crate::runtime` and `crate::runtime::reliability`.

mod discovery;
mod events;
mod gates;

pub use discovery::{
    Discovery, DiscoveryPriority, DiscoverySummary, DiscoveryTracker, ScopeBudget,
};
pub use events::{EventBuilder, EventId, Evidence, TaskEvent, TaskEventKind, TaskEventLog};
pub use gates::{
    CompletionGate, CompletionGates, EvidenceGate, FnGate, GateBuilder, GateRecord, GateResult,
    GateSeverity, GateSummary,
};

use serde::{Deserialize, Serialize};

/// The final result of a completed task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskResult {
    /// Task completed successfully.
    Success,

    /// Task failed with a reason.
    Failed {
        /// Human-readable explanation of why the task failed.
        reason: String,
    },

    /// Task timed out before completion.
    Timeout,
}

impl TaskResult {
    /// Returns true if the task succeeded.
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Returns true if the task failed (including timeout).
    pub const fn is_failure(&self) -> bool {
        !self.is_success()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_result_helpers() {
        assert!(TaskResult::Success.is_success());
        assert!(!TaskResult::Success.is_failure());

        assert!(!TaskResult::Timeout.is_success());
        assert!(TaskResult::Timeout.is_failure());

        let failed = TaskResult::Failed {
            reason: "test error".to_string(),
        };
        assert!(!failed.is_success());
        assert!(failed.is_failure());
    }
}
