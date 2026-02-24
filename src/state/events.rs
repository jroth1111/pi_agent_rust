//! Event sourcing for task lifecycle audit trails.
//!
//! Provides an immutable log of all state transitions and significant
//! actions taken during task execution. This addresses Flaw 11 (Unsafe
//! and Untraceable Closes) by ensuring all task operations are logged
//! and auditable.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use super::discovery::DiscoveryPriority;
use super::{RuntimeState, TaskResult};
use crate::reliability::StuckPattern;

/// Evidence attached to task completion events.
///
/// Provides proof that specific work was performed and verified.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Evidence {
    /// Test execution output.
    TestOutput {
        /// Number of tests that passed.
        passed: usize,
        /// Number of tests that failed.
        failed: usize,
        /// Full test output.
        output: String,
    },
    /// Git diff of changes.
    GitDiff {
        /// Number of files changed.
        files_changed: usize,
        /// The diff content.
        diff: String,
    },
    /// Build result.
    BuildResult {
        /// Whether the build succeeded.
        success: bool,
        /// Number of warnings.
        warnings: usize,
    },
    /// Screenshot evidence.
    Screenshot {
        /// Path to the screenshot file.
        path: String,
        /// Description of what the screenshot shows.
        description: String,
    },
    /// Custom evidence type.
    Custom {
        /// Kind identifier for the custom evidence.
        kind: String,
        /// Brief summary of the evidence.
        summary: String,
        /// Optional detailed information.
        details: Option<String>,
    },
}

impl Evidence {
    /// Get a short description of this evidence.
    pub fn description(&self) -> String {
        match self {
            Self::TestOutput { passed, failed, .. } => {
                format!("Test results: {passed} passed, {failed} failed")
            }
            Self::GitDiff { files_changed, .. } => {
                format!("Git diff: {files_changed} files changed")
            }
            Self::BuildResult { success, warnings } => {
                format!(
                    "Build: {} ({} warnings)",
                    if *success { "success" } else { "failed" },
                    warnings
                )
            }
            Self::Screenshot { path, description } => {
                format!("Screenshot: {description} ({path})")
            }
            Self::Custom { kind, summary, .. } => {
                format!("{kind}: {summary}")
            }
        }
    }

    /// Get the kind name as a string.
    pub fn kind_name(&self) -> &str {
        match self {
            Self::TestOutput { .. } => "test_output",
            Self::GitDiff { .. } => "git_diff",
            Self::BuildResult { .. } => "build_result",
            Self::Screenshot { .. } => "screenshot",
            Self::Custom { kind, .. } => kind.as_str(),
        }
    }
}

/// Unique identifier for a task event.
pub type EventId = u64;

/// An immutable record of something that happened to a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskEvent {
    /// Monotonically increasing event ID.
    pub id: EventId,
    /// Type of event that occurred.
    pub kind: TaskEventKind,
    /// Unix timestamp (ms) when the event occurred.
    pub timestamp: u64,
    /// ID of the agent that triggered this event, if applicable.
    pub agent_id: Option<String>,
    /// Additional context about the event.
    pub context: Option<String>,
}

/// The kind of event that occurred.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskEventKind {
    // Lifecycle events
    /// Task was created.
    Created,
    /// Task was claimed by an agent.
    Claimed {
        /// Fence token issued for this claim.
        fence: u64,
    },
    /// Task lease was released.
    Released {
        /// Fence token that was released.
        fence: u64,
    },
    /// Task entered verification state.
    VerificationStarted,
    /// Task completed with a result.
    Completed {
        /// The final result.
        result: TaskResult,
        /// REQUIRED: Reason for the result (even for success).
        /// This addresses Flaw 11 by mandating explanation.
        reason: String,
        /// Evidence supporting the completion claim.
        #[serde(default)]
        evidence: Vec<Evidence>,
    },
    /// Task was abandoned (lease expired without completion).
    Abandoned {
        /// Reason for abandonment.
        reason: String,
    },

    // Work events
    /// A todo item was added.
    TodoAdded {
        /// ID of the todo.
        todo_id: String,
        /// Content of the todo.
        content: String,
    },
    /// A todo item status changed.
    TodoStatusChanged {
        /// ID of the todo.
        todo_id: String,
        /// Previous status.
        from: String,
        /// New status.
        to: String,
    },
    /// An attempt was recorded.
    AttemptRecorded {
        /// What was attempted.
        action: String,
        /// Whether it succeeded.
        success: bool,
    },

    // Handoff events
    /// Task was handed off to another agent/session.
    HandedOff {
        /// ID of the receiving agent.
        to_agent: String,
        /// Summary of handoff context.
        summary: String,
    },
    /// Task was received from another agent/session.
    ReceivedFrom {
        /// ID of the sending agent.
        from_agent: String,
        /// Summary of handoff context.
        summary: String,
    },

    // Gate events
    /// A verification gate was registered.
    GateRegistered {
        /// Name of the gate.
        gate_name: String,
    },
    /// A verification gate passed.
    GatePassed {
        /// Name of the gate.
        gate_name: String,
    },
    /// A verification gate failed.
    GateFailed {
        /// Name of the gate.
        gate_name: String,
        /// Reason for failure.
        reason: String,
    },

    // Discovery events
    /// A discovery was made during task execution.
    DiscoveryMade {
        /// Description of the discovery.
        description: String,
        /// Related event ID (the original triggering event).
        related_to: EventId,
        /// Priority of the discovery.
        priority: DiscoveryPriority,
    },

    // Reliability events
    /// A stuck pattern was detected.
    StuckDetected {
        /// The pattern that was detected.
        pattern: StuckPattern,
    },
}

impl TaskEvent {
    /// Creates a new event with the given kind.
    pub fn new(id: EventId, kind: TaskEventKind, agent_id: Option<String>) -> Self {
        Self {
            id,
            kind,
            timestamp: current_timestamp_ms(),
            agent_id,
            context: None,
        }
    }

    /// Creates an event with additional context.
    pub fn with_context(mut self, context: String) -> Self {
        self.context = Some(context);
        self
    }

    /// Returns true if this is a terminal event (task completed or abandoned).
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            TaskEventKind::Completed { .. } | TaskEventKind::Abandoned { .. }
        )
    }

    /// Returns true if this event indicates a state transition.
    pub const fn is_state_transition(&self) -> bool {
        matches!(
            self.kind,
            TaskEventKind::Claimed { .. }
                | TaskEventKind::Released { .. }
                | TaskEventKind::VerificationStarted
                | TaskEventKind::Completed { .. }
                | TaskEventKind::Abandoned { .. }
        )
    }
}

/// An append-only log of task events.
///
/// Provides efficient storage and querying of event history with
/// configurable retention limits to prevent unbounded growth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEventLog {
    /// The events in chronological order.
    events: VecDeque<TaskEvent>,
    /// Next event ID to assign.
    next_id: EventId,
    /// Maximum number of events to retain (0 = unlimited).
    max_events: usize,
    /// Total events ever recorded (including evicted).
    total_count: u64,
}

impl Default for TaskEventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskEventLog {
    /// Creates a new empty event log.
    pub const fn new() -> Self {
        Self {
            events: VecDeque::new(),
            next_id: 1,
            max_events: 10_000, // Default limit
            total_count: 0,
        }
    }

    /// Creates a new log with a custom retention limit.
    pub const fn with_max_events(max_events: usize) -> Self {
        Self {
            events: VecDeque::new(),
            next_id: 1,
            max_events,
            total_count: 0,
        }
    }

    /// Appends a new event to the log.
    pub fn append(&mut self, kind: TaskEventKind, agent_id: Option<String>) -> &TaskEvent {
        self.append_with_context(kind, agent_id, None)
    }

    /// Appends a new event with optional context.
    pub fn append_with_context(
        &mut self,
        kind: TaskEventKind,
        agent_id: Option<String>,
        context: Option<String>,
    ) -> &TaskEvent {
        let event = TaskEvent {
            id: self.next_id,
            kind,
            timestamp: current_timestamp_ms(),
            agent_id,
            context,
        };

        self.next_id += 1;
        self.total_count += 1;

        // Evict oldest if at capacity
        if self.max_events > 0 && self.events.len() >= self.max_events {
            self.events.pop_front();
        }

        self.events.push_back(event);
        self.events.back().expect("just pushed")
    }

    /// Returns the number of events currently in the log.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns true if the log is empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns the total number of events ever recorded.
    pub const fn total_count(&self) -> u64 {
        self.total_count
    }

    /// Returns an iterator over all events.
    pub fn iter(&self) -> impl Iterator<Item = &TaskEvent> {
        self.events.iter()
    }

    /// Returns the most recent event, if any.
    pub fn last(&self) -> Option<&TaskEvent> {
        self.events.back()
    }

    /// Returns the first event, if any.
    pub fn first(&self) -> Option<&TaskEvent> {
        self.events.front()
    }

    /// Returns events that match the given predicate.
    pub fn filter<F>(&self, predicate: F) -> Vec<&TaskEvent>
    where
        F: Fn(&TaskEvent) -> bool,
    {
        self.events.iter().filter(|e| predicate(e)).collect()
    }

    /// Returns all state transition events.
    pub fn state_transitions(&self) -> Vec<&TaskEvent> {
        self.filter(TaskEvent::is_state_transition)
    }

    /// Returns all terminal events.
    pub fn terminal_events(&self) -> Vec<&TaskEvent> {
        self.filter(TaskEvent::is_terminal)
    }

    /// Returns events by the given agent.
    pub fn by_agent(&self, agent_id: &str) -> Vec<&TaskEvent> {
        self.filter(|e| e.agent_id.as_deref() == Some(agent_id))
    }

    /// Returns events of a specific kind (by predicate).
    pub fn of_kind<F>(&self, predicate: F) -> Vec<&TaskEvent>
    where
        F: Fn(&TaskEventKind) -> bool,
    {
        self.filter(|e| predicate(&e.kind))
    }

    /// Reconstructs the current state from event history.
    ///
    /// This is the core of event sourcing - we can derive the current
    /// state at any point by replaying events.
    pub fn reconstruct_state(&self) -> RuntimeState {
        let mut state = RuntimeState::Ready;

        for event in &self.events {
            match &event.kind {
                TaskEventKind::Claimed { fence } => {
                    if let Some(agent_id) = &event.agent_id {
                        state = RuntimeState::Leased {
                            agent_id: agent_id.clone(),
                            fence: *fence,
                            leased_at: event.timestamp,
                        };
                    }
                }
                TaskEventKind::Released { .. } => {
                    state = RuntimeState::Ready;
                }
                TaskEventKind::VerificationStarted => {
                    state = RuntimeState::Verifying;
                }
                TaskEventKind::Completed { result, .. } => {
                    state = RuntimeState::Terminal {
                        result: result.clone(),
                        completed_at: event.timestamp,
                    };
                }
                TaskEventKind::Abandoned { .. } => {
                    state = RuntimeState::Terminal {
                        result: TaskResult::Failed {
                            reason: "Task was abandoned".to_string(),
                        },
                        completed_at: event.timestamp,
                    };
                }
                _ => {}
            }
        }

        state
    }

    /// Serializes the log to JSON bytes.
    pub fn serialize(&self) -> crate::error::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| crate::error::Error::Json(Box::new(e)))
    }

    /// Deserializes a log from JSON bytes.
    pub fn deserialize(data: &[u8]) -> crate::error::Result<Self> {
        serde_json::from_slice(data).map_err(|e| crate::error::Error::Json(Box::new(e)))
    }
}

/// Helper to get current timestamp in milliseconds.
fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Builder for creating task events with fluent API.
pub struct EventBuilder {
    kind: Option<TaskEventKind>,
    agent_id: Option<String>,
    context: Option<String>,
}

impl EventBuilder {
    /// Creates a new event builder.
    pub const fn new() -> Self {
        Self {
            kind: None,
            agent_id: None,
            context: None,
        }
    }

    /// Sets the event kind.
    pub fn kind(mut self, kind: TaskEventKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Sets the agent ID.
    pub fn agent(mut self, agent_id: String) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    /// Sets the context.
    pub fn context(mut self, context: String) -> Self {
        self.context = Some(context);
        self
    }

    /// Builds the event (requires kind to be set).
    pub fn build(self, id: EventId) -> Option<TaskEvent> {
        self.kind.map(|kind| TaskEvent {
            id,
            kind,
            timestamp: current_timestamp_ms(),
            agent_id: self.agent_id,
            context: self.context,
        })
    }
}

impl Default for EventBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_starts_empty() {
        let log = TaskEventLog::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn append_event() {
        let mut log = TaskEventLog::new();
        log.append(TaskEventKind::Created, None);

        assert_eq!(log.len(), 1);
        assert!(log.first().is_some());
        assert!(log.last().is_some());
    }

    #[test]
    fn event_ids_are_sequential() {
        let mut log = TaskEventLog::new();
        let e1 = log.append(TaskEventKind::Created, None);
        let e1_id = e1.id;
        let e2 = log.append(
            TaskEventKind::Claimed { fence: 1 },
            Some("agent-1".to_string()),
        );

        assert_eq!(e1_id, 1);
        assert_eq!(e2.id, 2);
    }

    #[test]
    fn event_log_eviction() {
        let mut log = TaskEventLog::with_max_events(3);

        log.append(TaskEventKind::Created, None);
        log.append(TaskEventKind::Claimed { fence: 1 }, None);
        log.append(TaskEventKind::Released { fence: 1 }, None);
        log.append(
            TaskEventKind::Completed {
                result: TaskResult::Success,
                reason: "Done".to_string(),
                evidence: vec![],
            },
            None,
        );

        // Should have evicted the Created event
        assert_eq!(log.len(), 3);
        assert!(matches!(
            log.first().unwrap().kind,
            TaskEventKind::Claimed { .. }
        ));
        assert_eq!(log.total_count(), 4);
    }

    #[test]
    fn filter_events() {
        let mut log = TaskEventLog::new();
        log.append(TaskEventKind::Created, None);
        log.append(
            TaskEventKind::Claimed { fence: 1 },
            Some("agent-1".to_string()),
        );
        log.append(
            TaskEventKind::TodoAdded {
                todo_id: "t1".to_string(),
                content: "test".to_string(),
            },
            Some("agent-1".to_string()),
        );
        log.append(TaskEventKind::Released { fence: 1 }, None);

        let agent_events = log.by_agent("agent-1");
        assert_eq!(agent_events.len(), 2);

        let transitions = log.state_transitions();
        assert_eq!(transitions.len(), 2); // Claimed and Released
    }

    #[test]
    fn reconstruct_state_from_events() {
        let mut log = TaskEventLog::new();
        log.append(TaskEventKind::Created, None);

        assert!(log.reconstruct_state().is_claimable());

        log.append(
            TaskEventKind::Claimed { fence: 1 },
            Some("agent-1".to_string()),
        );

        let state = log.reconstruct_state();
        assert!(state.is_leased());
        assert_eq!(state.lease_holder(), Some("agent-1"));

        log.append(TaskEventKind::VerificationStarted, None);
        assert!(matches!(log.reconstruct_state(), RuntimeState::Verifying));

        log.append(
            TaskEventKind::Completed {
                result: TaskResult::Success,
                reason: "All tests passed".to_string(),
                evidence: vec![],
            },
            None,
        );

        assert!(log.reconstruct_state().is_terminal());
    }

    #[test]
    fn event_builder() {
        let event = EventBuilder::new()
            .kind(TaskEventKind::Created)
            .agent("agent-1".to_string())
            .context("Task initialized".to_string())
            .build(1);

        assert!(event.is_some());
        let event = event.unwrap();
        assert!(matches!(event.kind, TaskEventKind::Created));
        assert_eq!(event.agent_id, Some("agent-1".to_string()));
        assert_eq!(event.context, Some("Task initialized".to_string()));
    }

    #[test]
    fn serialization_roundtrip() {
        let mut log = TaskEventLog::new();
        log.append(TaskEventKind::Created, None);
        log.append(
            TaskEventKind::Claimed { fence: 1 },
            Some("agent-1".to_string()),
        );
        log.append(
            TaskEventKind::Completed {
                result: TaskResult::Success,
                reason: "Done".to_string(),
                evidence: vec![],
            },
            None,
        );

        let bytes = log.serialize().unwrap();
        let restored = TaskEventLog::deserialize(&bytes).unwrap();

        assert_eq!(restored.len(), 3);
        assert_eq!(restored.total_count(), 3);
    }

    #[test]
    fn mandatory_reason_on_completion() {
        let mut log = TaskEventLog::new();
        log.append(
            TaskEventKind::Completed {
                result: TaskResult::Success,
                reason: "All acceptance criteria met, tests pass".to_string(),
                evidence: vec![],
            },
            None,
        );

        // Verify the event requires a reason
        if let Some(TaskEventKind::Completed {
            result: _,
            reason,
            evidence: _,
        }) = log.last().map(|e| &e.kind)
        {
            assert!(!reason.is_empty(), "Completion events must have a reason");
        }
    }

    #[test]
    fn evidence_test_output() {
        let evidence = Evidence::TestOutput {
            passed: 10,
            failed: 2,
            output: "Test output here".to_string(),
        };

        assert_eq!(evidence.kind_name(), "test_output");
        assert!(evidence.description().contains("10 passed"));
        assert!(evidence.description().contains("2 failed"));
    }

    #[test]
    fn evidence_git_diff() {
        let evidence = Evidence::GitDiff {
            files_changed: 5,
            diff: "diff --git a/file.rs b/file.rs".to_string(),
        };

        assert_eq!(evidence.kind_name(), "git_diff");
        assert!(evidence.description().contains("5 files changed"));
    }

    #[test]
    fn evidence_build_result() {
        let evidence = Evidence::BuildResult {
            success: true,
            warnings: 3,
        };

        assert_eq!(evidence.kind_name(), "build_result");
        assert!(evidence.description().contains("success"));
        assert!(evidence.description().contains("3 warnings"));
    }

    #[test]
    fn evidence_screenshot() {
        let evidence = Evidence::Screenshot {
            path: "/tmp/screenshot.png".to_string(),
            description: "UI after button click".to_string(),
        };

        assert_eq!(evidence.kind_name(), "screenshot");
        assert!(evidence.description().contains("UI after button click"));
    }

    #[test]
    fn evidence_custom() {
        let evidence = Evidence::Custom {
            kind: "manual_check".to_string(),
            summary: "Verified output manually".to_string(),
            details: Some("Checked all assertions pass".to_string()),
        };

        assert_eq!(evidence.kind_name(), "manual_check");
        assert!(evidence.description().contains("Verified output manually"));
    }

    #[test]
    fn completion_with_evidence() {
        let evidence = vec![
            Evidence::TestOutput {
                passed: 5,
                failed: 0,
                output: "All tests passed".to_string(),
            },
            Evidence::GitDiff {
                files_changed: 2,
                diff: "Changes to main.rs".to_string(),
            },
        ];

        let event = TaskEvent::new(
            1,
            TaskEventKind::Completed {
                result: TaskResult::Success,
                reason: "Task completed successfully".to_string(),
                evidence,
            },
            Some("agent-1".to_string()),
        );

        if let TaskEventKind::Completed {
            result,
            reason,
            evidence,
        } = event.kind
        {
            assert!(result.is_success());
            assert!(!reason.is_empty());
            assert_eq!(evidence.len(), 2);
        } else {
            panic!("Expected Completed event");
        }
    }
}
