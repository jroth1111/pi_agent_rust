//! Session handoff context for transferring task state between agents.
//!
//! Provides a structured way to capture and restore context when
//! handing off a task from one agent/session to another.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use super::ledger::TodoItem;

/// Type alias for artifact IDs - using String for serde compatibility.
pub type ArtifactId = String;

/// Helper to generate a new artifact ID.
fn new_artifact_id() -> ArtifactId {
    Uuid::new_v4().to_string()
}

/// Reference to an artifact created or modified during task execution.
///
/// Artifacts are stored by reference rather than inline to keep
/// handoff context lightweight and avoid duplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Unique identifier for this artifact.
    pub id: ArtifactId,
    /// Type of artifact (e.g., "file", "commit", "screenshot").
    pub artifact_type: String,
    /// Human-readable description of the artifact.
    pub description: String,
    /// Path or URI where the artifact can be found.
    pub location: String,
    /// Unix timestamp (ms) when the artifact was created.
    pub created_at: u64,
}

impl ArtifactRef {
    /// Creates a new artifact reference.
    pub fn new(artifact_type: String, description: String, location: String, now: u64) -> Self {
        Self {
            id: new_artifact_id(),
            artifact_type,
            description,
            location,
            created_at: now,
        }
    }

    /// Creates a file artifact reference.
    pub fn file(path: PathBuf, description: String, now: u64) -> Self {
        Self {
            id: new_artifact_id(),
            artifact_type: "file".to_string(),
            description,
            location: path.to_string_lossy().to_string(),
            created_at: now,
        }
    }

    /// Creates a commit artifact reference.
    pub fn commit(sha: String, description: String, now: u64) -> Self {
        Self {
            id: new_artifact_id(),
            artifact_type: "commit".to_string(),
            description,
            location: sha,
            created_at: now,
        }
    }
}

/// Context captured when handing off a task from one session to another.
///
/// This structure addresses Flaw 15 by providing all the information
/// needed for a new agent to continue work on a task without starting
/// from scratch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffContext {
    /// ID of the session being handed off.
    pub session_id: String,

    /// Description of the current task being worked on.
    pub current_task: String,

    /// Current todo items and their status.
    pub todos: Vec<TodoItem>,

    // === What did I try? ===
    /// Description of the last action taken.
    pub last_action: String,

    /// Any error observed in the last attempt.
    pub observed_error: Option<String>,

    /// Dead ends encountered - things that didn't work.
    /// Format: "Tried X, failed with Y"
    pub dead_ends: Vec<String>,

    // === What's next? ===
    /// Suggested next step for the receiving agent.
    pub suggested_next_step: String,

    // === Where am I? ===
    /// Current working directory.
    pub working_directory: PathBuf,

    /// Estimated percentage of original context that can be restored (0-100).
    /// Values above 80.0 suggest the context may be too large to fully restore.
    pub context_percentage: f32,

    // === Evidence (as refs, not inline) ===
    /// References to artifacts created during the task.
    pub artifacts: Vec<ArtifactRef>,
}

impl HandoffContext {
    /// Creates a new handoff context with minimal required fields.
    pub const fn new(session_id: String, current_task: String, working_directory: PathBuf) -> Self {
        Self {
            session_id,
            current_task,
            todos: Vec::new(),
            last_action: String::new(),
            observed_error: None,
            dead_ends: Vec::new(),
            suggested_next_step: String::new(),
            working_directory,
            context_percentage: 0.0,
            artifacts: Vec::new(),
        }
    }

    /// Creates a new handoff context with a generated session ID.
    pub fn new_with_id(current_task: String, working_directory: PathBuf) -> Self {
        Self::new(Uuid::new_v4().to_string(), current_task, working_directory)
    }

    /// Returns true if the context can be meaningfully restored.
    ///
    /// Context restoration becomes problematic when context_percentage
    /// exceeds 80%, as the accumulated state may be too large or
    /// complex to transfer effectively.
    pub fn can_restore(&self) -> bool {
        self.context_percentage < 80.0
    }

    /// Returns true if there are dead ends that should be avoided.
    pub fn has_dead_ends(&self) -> bool {
        !self.dead_ends.is_empty()
    }

    /// Returns true if the last action resulted in an error.
    pub const fn has_error(&self) -> bool {
        self.observed_error.is_some()
    }

    /// Returns the count of open (incomplete) todos.
    pub fn open_todo_count(&self) -> usize {
        self.todos.iter().filter(|t| t.is_open()).count()
    }

    /// Returns true if all todos are completed.
    pub fn is_complete(&self) -> bool {
        self.todos.iter().all(|t| !t.is_open())
    }

    /// Adds a dead end to the list.
    pub fn add_dead_end(&mut self, action: &str, failure: &str) {
        self.dead_ends
            .push(format!("Tried {action}, failed with {failure}"));
    }

    /// Adds an artifact reference.
    pub fn add_artifact(&mut self, artifact: ArtifactRef) {
        self.artifacts.push(artifact);
    }

    /// Sets the last action and optionally an observed error.
    pub fn set_last_action(&mut self, action: String, error: Option<String>) {
        self.last_action = action;
        self.observed_error = error;
    }

    /// Serializes the context to JSON bytes.
    pub fn serialize(&self) -> crate::error::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| crate::error::Error::Json(Box::new(e)))
    }

    /// Deserializes context from JSON bytes.
    pub fn deserialize(data: &[u8]) -> crate::error::Result<Self> {
        serde_json::from_slice(data).map_err(|e| crate::error::Error::Json(Box::new(e)))
    }

    /// Serializes to a JSON string.
    pub fn to_json_string(&self) -> crate::error::Result<String> {
        serde_json::to_string(self).map_err(|e| crate::error::Error::Json(Box::new(e)))
    }

    /// Deserializes from a JSON string.
    pub fn from_json_string(s: &str) -> crate::error::Result<Self> {
        serde_json::from_str(s).map_err(|e| crate::error::Error::Json(Box::new(e)))
    }

    /// Creates a summary of this handoff for logging/display.
    pub fn summary(&self) -> HandoffSummary {
        HandoffSummary {
            session_id: self.session_id.clone(),
            task: self.current_task.clone(),
            open_todos: self.open_todo_count(),
            total_todos: self.todos.len(),
            has_error: self.has_error(),
            dead_ends: self.dead_ends.len(),
            artifacts: self.artifacts.len(),
            can_restore: self.can_restore(),
        }
    }
}

/// A lightweight summary of a handoff context for logging/display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffSummary {
    /// Session ID being handed off.
    pub session_id: String,
    /// Task description (may be truncated).
    pub task: String,
    /// Number of open todos.
    pub open_todos: usize,
    /// Total number of todos.
    pub total_todos: usize,
    /// Whether there was an error in the last action.
    pub has_error: bool,
    /// Number of dead ends encountered.
    pub dead_ends: usize,
    /// Number of artifacts created.
    pub artifacts: usize,
    /// Whether the context can be fully restored.
    pub can_restore: bool,
}

impl std::fmt::Display for HandoffSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Handoff[session={}, task='{}', todos={}/{}, errors={}, dead_ends={}, artifacts={}, restorable={}]",
            self.session_id,
            if self.task.len() > 50 {
                format!("{}...", &self.task[..47])
            } else {
                self.task.clone()
            },
            self.open_todos,
            self.total_todos,
            self.has_error,
            self.dead_ends,
            self.artifacts,
            self.can_restore
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ledger::TodoStatus;

    fn make_context() -> HandoffContext {
        HandoffContext::new_with_id(
            "Implement feature X".to_string(),
            PathBuf::from("/project/src"),
        )
    }

    #[test]
    fn new_context_is_restorable() {
        let ctx = make_context();
        assert!(ctx.can_restore());
        assert!(!ctx.has_dead_ends());
        assert!(!ctx.has_error());
    }

    #[test]
    fn context_with_high_percentage_not_restorable() {
        let mut ctx = make_context();
        ctx.context_percentage = 85.0;
        assert!(!ctx.can_restore());
    }

    #[test]
    fn add_dead_end() {
        let mut ctx = make_context();
        ctx.add_dead_end("using library X", "version conflict");

        assert!(ctx.has_dead_ends());
        assert_eq!(ctx.dead_ends.len(), 1);
        assert!(ctx.dead_ends[0].contains("using library X"));
        assert!(ctx.dead_ends[0].contains("version conflict"));
    }

    #[test]
    fn set_last_action_with_error() {
        let mut ctx = make_context();
        ctx.set_last_action("ran tests".to_string(), Some("3 tests failed".to_string()));

        assert_eq!(ctx.last_action, "ran tests");
        assert!(ctx.has_error());
        assert_eq!(ctx.observed_error, Some("3 tests failed".to_string()));
    }

    #[test]
    fn open_todo_count() {
        let mut ctx = make_context();
        ctx.todos.push(TodoItem {
            id: "todo-1".to_string(),
            content: "Task 1".to_string(),
            status: TodoStatus::Completed,
            created_at: 1000,
            updated_at: 2000,
        });
        ctx.todos.push(TodoItem {
            id: "todo-2".to_string(),
            content: "Task 2".to_string(),
            status: TodoStatus::Pending,
            created_at: 1000,
            updated_at: 1000,
        });

        assert_eq!(ctx.open_todo_count(), 1);
        assert!(!ctx.is_complete());
    }

    #[test]
    fn is_complete_when_all_done() {
        let mut ctx = make_context();
        ctx.todos.push(TodoItem {
            id: "todo-1".to_string(),
            content: "Task 1".to_string(),
            status: TodoStatus::Completed,
            created_at: 1000,
            updated_at: 2000,
        });

        assert!(ctx.is_complete());
    }

    #[test]
    fn artifact_ref_file() {
        let artifact = ArtifactRef::file(
            PathBuf::from("/src/main.rs"),
            "Main entry point".to_string(),
            1000,
        );

        assert_eq!(artifact.artifact_type, "file");
        assert_eq!(artifact.location, "/src/main.rs");
    }

    #[test]
    fn artifact_ref_commit() {
        let artifact =
            ArtifactRef::commit("abc123".to_string(), "Initial commit".to_string(), 1000);

        assert_eq!(artifact.artifact_type, "commit");
        assert_eq!(artifact.location, "abc123");
    }

    #[test]
    fn serialization_roundtrip() {
        let mut ctx = make_context();
        ctx.todos.push(TodoItem {
            id: "todo-1".to_string(),
            content: "Test".to_string(),
            status: TodoStatus::Pending,
            created_at: 1000,
            updated_at: 1000,
        });
        ctx.add_dead_end("approach A", "didn't work");
        ctx.add_artifact(ArtifactRef::file(
            PathBuf::from("/test.txt"),
            "Test file".to_string(),
            1000,
        ));

        let bytes = ctx.serialize().unwrap();
        let restored = HandoffContext::deserialize(&bytes).unwrap();

        assert_eq!(restored.todos.len(), 1);
        assert_eq!(restored.dead_ends.len(), 1);
        assert_eq!(restored.artifacts.len(), 1);
    }

    #[test]
    fn summary_display() {
        let mut ctx = make_context();
        ctx.context_percentage = 50.0;
        ctx.todos.push(TodoItem {
            id: "todo-1".to_string(),
            content: "Task".to_string(),
            status: TodoStatus::Pending,
            created_at: 1000,
            updated_at: 1000,
        });

        let summary = ctx.summary();
        let display = format!("{}", summary);

        assert!(display.contains("session="));
        assert!(display.contains("todos=1/1"));
        assert!(display.contains("restorable=true"));
    }
}
