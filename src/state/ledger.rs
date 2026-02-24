//! Task ledger for durable task state management.
//!
//! Supports efficient operations for up to 2000+ tasks using
//! index-based lookups with HashMap acceleration.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Type alias for todo IDs - using String for serde compatibility.
pub type TodoId = String;

/// Helper to generate a new todo ID.
fn new_todo_id() -> TodoId {
    Uuid::new_v4().to_string()
}

/// Status of a todo item in the task ledger.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum TodoStatus {
    /// Item is pending and not yet started.
    #[default]
    Pending,
    /// Item is currently being worked on.
    InProgress,
    /// Item has been completed.
    Completed,
}

/// A single todo item in the task ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    /// Unique identifier for this todo.
    pub id: TodoId,
    /// Human-readable description of the todo.
    pub content: String,
    /// Current status of the todo.
    pub status: TodoStatus,
    /// Unix timestamp (ms) when this todo was created.
    pub created_at: u64,
    /// Unix timestamp (ms) when this todo was last updated.
    pub updated_at: u64,
}

impl TodoItem {
    /// Creates a new todo item with the given content.
    pub fn new(content: String, now: u64) -> Self {
        Self {
            id: new_todo_id(),
            content,
            status: TodoStatus::Pending,
            created_at: now,
            updated_at: now,
        }
    }

    /// Marks this todo as in progress.
    pub const fn start(&mut self, now: u64) {
        self.status = TodoStatus::InProgress;
        self.updated_at = now;
    }

    /// Marks this todo as completed.
    pub const fn complete(&mut self, now: u64) {
        self.status = TodoStatus::Completed;
        self.updated_at = now;
    }

    /// Returns true if this todo is not yet completed.
    pub fn is_open(&self) -> bool {
        self.status != TodoStatus::Completed
    }
}

/// Outcome of an attempt to complete work.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The attempt succeeded.
    Success,
    /// The attempt failed with a reason.
    Failed { reason: String },
    /// The attempt was blocked by something.
    Blocked { blocker: String },
}

/// Record of an attempt to make progress on the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    /// What action was attempted.
    pub action: String,
    /// The outcome of the attempt.
    pub outcome: AttemptOutcome,
    /// Unix timestamp (ms) when the attempt was made.
    pub attempted_at: u64,
    /// Any additional notes or context.
    pub notes: Option<String>,
}

impl Attempt {
    /// Creates a new attempt record.
    pub const fn new(action: String, outcome: AttemptOutcome, now: u64) -> Self {
        Self {
            action,
            outcome,
            attempted_at: now,
            notes: None,
        }
    }

    /// Creates a new attempt with additional notes.
    pub const fn with_notes(
        action: String,
        outcome: AttemptOutcome,
        now: u64,
        notes: String,
    ) -> Self {
        Self {
            action,
            outcome,
            attempted_at: now,
            notes: Some(notes),
        }
    }

    /// Returns true if this attempt succeeded.
    pub const fn is_success(&self) -> bool {
        matches!(self.outcome, AttemptOutcome::Success)
    }
}

/// Durable ledger tracking task state, todos, and attempts.
///
/// Optimized for efficient operations with 2000+ items using:
/// - Vec storage for memory locality
/// - HashMap index for O(1) lookups by ID
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLedger {
    /// All todo items, indexed for O(1) access.
    todos: Vec<TodoItem>,
    /// Record of attempts made on this task.
    attempts: Vec<Attempt>,
    /// Current runtime state.
    #[serde(default)]
    state: super::RuntimeState,
    /// Index mapping todo IDs to their position in the vec.
    #[serde(skip)]
    todo_index: HashMap<TodoId, usize>,
}

impl Default for TaskLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskLedger {
    /// Creates a new empty ledger.
    pub fn new() -> Self {
        Self {
            todos: Vec::new(),
            attempts: Vec::new(),
            state: super::RuntimeState::Ready,
            todo_index: HashMap::new(),
        }
    }

    /// Creates a new ledger with pre-allocated capacity for efficiency.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            todos: Vec::with_capacity(capacity),
            attempts: Vec::new(),
            state: super::RuntimeState::Ready,
            todo_index: HashMap::with_capacity(capacity),
        }
    }

    /// Rebuilds the index after deserialization.
    ///
    /// MUST be called after deserializing a TaskLedger, as the index
    /// is skipped during serialization.
    pub fn rebuild_index(&mut self) {
        self.todo_index.clear();
        self.todo_index.reserve(self.todos.len());
        for (idx, todo) in self.todos.iter().enumerate() {
            self.todo_index.insert(todo.id.clone(), idx);
        }
    }

    /// Returns the current runtime state.
    pub const fn state(&self) -> &super::RuntimeState {
        &self.state
    }

    /// Returns a mutable reference to the runtime state.
    pub const fn state_mut(&mut self) -> &mut super::RuntimeState {
        &mut self.state
    }

    // === Todo Operations (O(1) with index) ===

    /// Adds a new todo item. Returns the ID of the created todo.
    pub fn add_todo(&mut self, content: String, now: u64) -> TodoId {
        let todo = TodoItem::new(content, now);
        let id = todo.id.clone();
        let idx = self.todos.len();
        self.todos.push(todo);
        self.todo_index.insert(id.clone(), idx);
        id
    }

    /// Gets a todo by ID. Returns None if not found.
    pub fn get_todo(&self, id: &str) -> Option<&TodoItem> {
        self.todo_index.get(id).map(|&idx| &self.todos[idx])
    }

    /// Gets a mutable reference to a todo by ID.
    pub fn get_todo_mut(&mut self, id: &str) -> Option<&mut TodoItem> {
        if let Some(&idx) = self.todo_index.get(id) {
            Some(&mut self.todos[idx])
        } else {
            None
        }
    }

    /// Marks a todo as completed. Returns false if not found.
    pub fn complete_todo(&mut self, id: &str, now: u64) -> bool {
        if let Some(todo) = self.get_todo_mut(id) {
            todo.complete(now);
            true
        } else {
            false
        }
    }

    /// Marks a todo as in progress. Returns false if not found.
    pub fn start_todo(&mut self, id: &str, now: u64) -> bool {
        if let Some(todo) = self.get_todo_mut(id) {
            todo.start(now);
            true
        } else {
            false
        }
    }

    /// Returns the total number of todos.
    pub fn todo_count(&self) -> usize {
        self.todos.len()
    }

    /// Returns the number of completed todos.
    pub fn completed_count(&self) -> usize {
        self.todos.iter().filter(|t| !t.is_open()).count()
    }

    /// Returns an iterator over all todos.
    pub fn todos(&self) -> impl Iterator<Item = &TodoItem> {
        self.todos.iter()
    }

    /// Returns an iterator over open (non-completed) todos.
    pub fn open_todos(&self) -> impl Iterator<Item = &TodoItem> {
        self.todos.iter().filter(|t| t.is_open())
    }

    /// Returns the number of open todos.
    pub fn open_todo_count(&self) -> usize {
        self.todos.iter().filter(|t| t.is_open()).count()
    }

    // === Attempt Operations ===

    /// Records an attempt on this task.
    pub fn record_attempt(&mut self, attempt: Attempt) {
        self.attempts.push(attempt);
    }

    /// Records a failed attempt with the given action and reason.
    pub fn record_failure(&mut self, action: String, reason: String, now: u64) {
        self.attempts
            .push(Attempt::new(action, AttemptOutcome::Failed { reason }, now));
    }

    /// Records a blocked attempt.
    pub fn record_blocked(&mut self, action: String, blocker: String, now: u64) {
        self.attempts.push(Attempt::new(
            action,
            AttemptOutcome::Blocked { blocker },
            now,
        ));
    }

    /// Records a successful attempt.
    pub fn record_success(&mut self, action: String, now: u64) {
        self.attempts
            .push(Attempt::new(action, AttemptOutcome::Success, now));
    }

    /// Returns the number of attempts.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// Returns the last attempt, if any.
    pub fn last_attempt(&self) -> Option<&Attempt> {
        self.attempts.last()
    }

    /// Returns an iterator over all attempts.
    pub fn attempts(&self) -> impl Iterator<Item = &Attempt> {
        self.attempts.iter()
    }

    // === Validation ===

    /// Validates that the task is ready for completion.
    ///
    /// Returns an error if there are open todos or if the state
    /// doesn't allow completion.
    pub fn validate_for_completion(&self) -> Result<()> {
        let open_count = self.open_todo_count();
        if open_count > 0 {
            return Err(Error::Validation(format!(
                "Cannot complete task: {open_count} open todos remain"
            )));
        }

        if self.state.is_terminal() {
            return Err(Error::Validation(
                "Cannot complete task: already in terminal state".to_string(),
            ));
        }

        Ok(())
    }

    /// Checks if all todos are complete and the task can be marked done.
    pub fn is_fully_complete(&self) -> bool {
        self.open_todo_count() == 0
    }

    // === Serialization ===

    /// Serializes the ledger to JSON bytes for persistence.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| Error::Json(Box::new(e)))
    }

    /// Deserializes a ledger from JSON bytes.
    ///
    /// IMPORTANT: Caller must call `rebuild_index()` after deserialization
    /// if using the returned ledger for operations.
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        let mut ledger: Self =
            serde_json::from_slice(data).map_err(|e| Error::Json(Box::new(e)))?;
        ledger.rebuild_index();
        Ok(ledger)
    }

    /// Serializes to a JSON string.
    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| Error::Json(Box::new(e)))
    }

    /// Deserializes from a JSON string.
    pub fn from_json_string(s: &str) -> Result<Self> {
        let mut ledger: Self = serde_json::from_str(s).map_err(|e| Error::Json(Box::new(e)))?;
        ledger.rebuild_index();
        Ok(ledger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ledger_with_todos(count: usize) -> TaskLedger {
        let mut ledger = TaskLedger::with_capacity(count);
        for i in 0..count {
            ledger.add_todo(format!("Todo {i}"), 1000 + i as u64);
        }
        ledger
    }

    #[test]
    fn ledger_starts_empty() {
        let ledger = TaskLedger::new();
        assert_eq!(ledger.todo_count(), 0);
        assert_eq!(ledger.attempt_count(), 0);
        assert!(ledger.state().is_claimable());
    }

    #[test]
    fn add_and_get_todo() {
        let mut ledger = TaskLedger::new();
        let id = ledger.add_todo("Write tests".to_string(), 1000);

        let todo = ledger.get_todo(&id).expect("todo should exist");
        assert_eq!(todo.content, "Write tests");
        assert_eq!(todo.status, TodoStatus::Pending);
    }

    #[test]
    fn complete_todo() {
        let mut ledger = TaskLedger::new();
        let id = ledger.add_todo("Write tests".to_string(), 1000);

        assert!(ledger.complete_todo(&id, 2000));

        let todo = ledger.get_todo(&id).expect("todo should exist");
        assert_eq!(todo.status, TodoStatus::Completed);
        assert!(!todo.is_open());
    }

    #[test]
    fn get_nonexistent_todo() {
        let ledger = TaskLedger::new();
        assert!(ledger.get_todo("nonexistent-id").is_none());
    }

    #[test]
    fn open_todos_filter() {
        let mut ledger = TaskLedger::new();
        let id1 = ledger.add_todo("Todo 1".to_string(), 1000);
        let _id2 = ledger.add_todo("Todo 2".to_string(), 1001);
        ledger.complete_todo(&id1, 2000);

        let open: Vec<_> = ledger.open_todos().collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].content, "Todo 2");
    }

    #[test]
    fn record_attempts() {
        let mut ledger = TaskLedger::new();
        ledger.record_success("First try".to_string(), 1000);
        ledger.record_failure("Second try".to_string(), "Failed".to_string(), 2000);

        assert_eq!(ledger.attempt_count(), 2);
        assert!(ledger.last_attempt().unwrap().outcome != AttemptOutcome::Success);
    }

    #[test]
    fn validate_for_completion_with_open_todos() {
        let mut ledger = TaskLedger::new();
        ledger.add_todo("Must do this".to_string(), 1000);

        let result = ledger.validate_for_completion();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("open todos"));
    }

    #[test]
    fn validate_for_completion_all_done() {
        let mut ledger = TaskLedger::new();
        let id = ledger.add_todo("Done".to_string(), 1000);
        ledger.complete_todo(&id, 2000);

        let result = ledger.validate_for_completion();
        assert!(result.is_ok());
    }

    #[test]
    fn serialization_roundtrip() {
        let mut ledger = TaskLedger::new();
        let id1 = ledger.add_todo("Todo 1".to_string(), 1000);
        let _id2 = ledger.add_todo("Todo 2".to_string(), 1001);
        ledger.complete_todo(&id1, 2000);
        ledger.record_success("Attempted".to_string(), 1500);

        let json = ledger.serialize().unwrap();
        let restored = TaskLedger::deserialize(&json).unwrap();

        assert_eq!(restored.todo_count(), 2);
        assert_eq!(restored.completed_count(), 1);
        assert_eq!(restored.attempt_count(), 1);
    }

    #[test]
    fn large_ledger_efficiency() {
        // Test with 2000+ items to verify O(1) operations scale
        let count = 2500;
        let mut ledger = make_ledger_with_todos(count);

        assert_eq!(ledger.todo_count(), count);

        // Get operations should be O(1)
        let first_id = ledger.todos().next().unwrap().id.clone();
        let last_id = ledger.todos().last().unwrap().id.clone();

        assert!(ledger.get_todo(&first_id).is_some());
        assert!(ledger.get_todo(&last_id).is_some());

        // Complete half
        let todo_ids = ledger
            .todos()
            .take(count / 2)
            .map(|todo| todo.id.clone())
            .collect::<Vec<_>>();
        for id in &todo_ids {
            ledger.complete_todo(id, 3000);
        }

        assert_eq!(ledger.completed_count(), todo_ids.len());
        assert_eq!(ledger.open_todo_count(), count - count / 2);
    }

    #[test]
    fn json_string_roundtrip() {
        let mut ledger = TaskLedger::new();
        ledger.add_todo("Test".to_string(), 1000);

        let json = ledger.to_json_string().unwrap();
        let restored = TaskLedger::from_json_string(&json).unwrap();

        assert_eq!(restored.todo_count(), 1);
    }
}
