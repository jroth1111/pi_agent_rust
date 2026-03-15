//! State management for task lifecycle and runtime tracking.
//!
//! This module provides durable task state management supporting efficient
//! operations for up to 2000+ tasks. It addresses:
//! - Flaw 3: Typed runtime state (not stringly-typed)
//! - Flaw 11: Durable task state with ledger + mandatory close reason
//! - Flaw 15: Session handoff context
//!
//! ## Event Sourcing
//!
//! All state transitions are logged to an immutable event log (`TaskEventLog`)
//! that can be used to reconstruct state and audit task lifecycle.
//!
//! ## Completion Gates
//!
//! Tasks can have verification gates that must pass before completion.
//! This addresses Flaw 9 (Unverifiable Claims) by requiring evidence.

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

/// Runtime state for a task, representing its lifecycle position.
///
/// Uses fence tokens for optimistic concurrency control to prevent
/// lost updates when multiple agents interact with the same task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum RuntimeState {
    /// Task is available for any agent to claim.
    #[default]
    Ready,

    /// Task has been claimed by an agent with a fence token for concurrency control.
    Leased {
        /// ID of the agent that claimed this task.
        agent_id: String,
        /// Monotonically increasing fence token for optimistic concurrency.
        fence: u64,
        /// Unix timestamp (ms) when the lease was acquired.
        leased_at: u64,
    },

    /// Task is currently being verified (e.g., running tests, checks).
    Verifying,

    /// Task has reached a terminal state and will not transition further.
    Terminal {
        /// The final result of the task.
        result: TaskResult,
        /// Unix timestamp (ms) when the task reached terminal state.
        completed_at: u64,
    },
}

impl RuntimeState {
    /// Returns true if the task can be claimed by an agent.
    pub const fn is_claimable(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Returns true if the task is currently leased.
    pub const fn is_leased(&self) -> bool {
        matches!(self, Self::Leased { .. })
    }

    /// Returns true if the task is in a terminal state.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }

    /// Returns the agent ID if leased, None otherwise.
    pub fn lease_holder(&self) -> Option<&str> {
        match self {
            Self::Leased { agent_id, .. } => Some(agent_id),
            _ => None,
        }
    }

    /// Attempts to claim the task with the given agent ID.
    /// Returns `Ok(new_state)` on success, `Err(current_state)` if not claimable.
    pub fn claim(&self, agent_id: String, now: u64) -> std::result::Result<Self, Self> {
        if self.is_claimable() {
            Ok(Self::Leased {
                agent_id,
                fence: 1,
                leased_at: now,
            })
        } else {
            Err(self.clone())
        }
    }

    /// Attempts to release the lease. Validates the fence token for optimistic concurrency.
    /// Returns `Ok(new_state)` on success, `Err(current_state)` if fence mismatch or not leased.
    pub fn release(&self, expected_fence: u64) -> std::result::Result<Self, Self> {
        match self {
            Self::Leased { fence, .. } if *fence == expected_fence => Ok(Self::Ready),
            _ => Err(self.clone()),
        }
    }

    /// Transitions to Verifying state. Validates fence token.
    pub fn start_verification(&self, expected_fence: u64) -> std::result::Result<Self, Self> {
        match self {
            Self::Leased { fence, .. } if *fence == expected_fence => Ok(Self::Verifying),
            _ => Err(self.clone()),
        }
    }

    /// Transitions to Terminal state with the given result.
    pub fn complete(&self, result: TaskResult, now: u64) -> std::result::Result<Self, Self> {
        match self {
            Self::Verifying | Self::Leased { .. } => Ok(Self::Terminal {
                result,
                completed_at: now,
            }),
            Self::Ready => Err(self.clone()), // Can't complete unclaimed task
            Self::Terminal { .. } => Err(self.clone()), // Already terminal
        }
    }
}

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
    fn runtime_state_default_is_ready() {
        let state = RuntimeState::default();
        assert!(state.is_claimable());
        assert!(!state.is_leased());
        assert!(!state.is_terminal());
    }

    #[test]
    fn runtime_state_claim_transition() {
        let state = RuntimeState::Ready;
        let claimed = state.claim("agent-1".to_string(), 1000).unwrap();

        assert!(claimed.is_leased());
        assert_eq!(claimed.lease_holder(), Some("agent-1"));
        assert!(!claimed.is_claimable());
    }

    #[test]
    fn runtime_state_cannot_double_claim() {
        let state = RuntimeState::Ready;
        let claimed = state.claim("agent-1".to_string(), 1000).unwrap();

        // Already claimed, should fail
        let result = claimed.claim("agent-2".to_string(), 2000);
        assert!(result.is_err());
    }

    #[test]
    fn runtime_state_release_with_fence() {
        let state = RuntimeState::Ready;
        let claimed = state.claim("agent-1".to_string(), 1000).unwrap();

        // Wrong fence token
        let result = claimed.release(999);
        assert!(result.is_err());

        // Correct fence token
        let released = claimed.release(1).unwrap();
        assert!(released.is_claimable());
    }

    #[test]
    fn runtime_state_complete_from_verifying() {
        let state = RuntimeState::Ready;
        let claimed = state.claim("agent-1".to_string(), 1000).unwrap();
        let verifying = claimed.start_verification(1).unwrap();
        let terminal = verifying.complete(TaskResult::Success, 3000).unwrap();

        assert!(terminal.is_terminal());
    }

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
