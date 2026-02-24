//! Task Graph - High-level task orchestration with dependency management.
//!
//! This module provides a task graph abstraction for coordinating work
//! across multiple agents with proper dependency tracking, cycle detection,
//! and lease-based ownership.
//!
//! ## Key Types
//!
//! - [`TaskGraph`] - Core graph structure holding tasks and edges
//! - [`Task`] - Individual task with state tracking
//! - [`Edge`] - Dependency relationship between tasks
//! - [`DependencyType`] - Kinds of dependencies (blocks, parent-child, etc.)
//!
//! ## Features
//!
//! - O(1) task lookups via index
//! - Cycle detection with detailed cycle reporting
//! - Topological sort for execution ordering
//! - Transitive dependency/dependent queries
//! - Lease expiration and recovery support

pub mod dag;
pub mod recovery;

pub use dag::{Cycle, DependencyType, Edge, Trigger};
pub use recovery::{AgentState, DeferredTask};

use crate::reliability::state::RuntimeState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unique identifier for a task.
pub type TaskId = String;

/// Core task graph structure holding tasks and their dependencies.
#[derive(Debug, Clone, Default)]
pub struct TaskGraph {
    /// All tasks indexed by their ID.
    tasks: HashMap<TaskId, Task>,
    /// All edges (dependencies) in the graph.
    edges: Vec<Edge>,
    /// Index mapping task IDs to their position for O(1) lookups.
    /// This is a cache for fast access when iterating.
    task_index: HashMap<TaskId, usize>,
}

/// A single task in the graph with its current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier for this task.
    pub id: TaskId,
    /// Brief description of what the task does.
    pub subject: String,
    /// Detailed description of the task requirements.
    pub description: String,
    /// Current runtime state of the task.
    pub state: RuntimeState,
    /// Agent that currently owns this task (if any).
    pub owner: Option<String>,
    /// Creation timestamp (Unix seconds).
    pub created_at: u64,
    /// Optional priority for scheduling (higher = more important).
    pub priority: i32,
    /// Optional labels for filtering/categorization.
    pub labels: Vec<String>,
}

impl Task {
    /// Create a new task with the given ID and subject.
    pub fn new(id: TaskId, subject: String, description: String) -> Self {
        Self {
            id,
            subject,
            description,
            state: RuntimeState::Ready,
            owner: None,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            priority: 0,
            labels: Vec::new(),
        }
    }

    /// Check if this task is in a terminal state.
    pub const fn is_terminal(&self) -> bool {
        matches!(self.state, RuntimeState::Terminal(_))
    }

    /// Check if this task is ready to be claimed.
    pub const fn is_ready(&self) -> bool {
        matches!(self.state, RuntimeState::Ready)
    }

    /// Check if this task is blocked by dependencies.
    pub const fn is_blocked(&self) -> bool {
        matches!(self.state, RuntimeState::Blocked { .. })
    }

    /// Check if this task is currently leased by an agent.
    pub const fn is_leased(&self) -> bool {
        matches!(self.state, RuntimeState::Leased { .. })
    }
}

impl TaskGraph {
    /// Create a new empty task graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a task graph with pre-allocated capacity.
    pub fn with_capacity(tasks: usize, edges: usize) -> Self {
        Self {
            tasks: HashMap::with_capacity(tasks),
            edges: Vec::with_capacity(edges),
            task_index: HashMap::with_capacity(tasks),
        }
    }

    /// Add a task to the graph.
    ///
    /// Returns `false` if a task with the same ID already exists.
    pub fn add_task(&mut self, task: Task) -> bool {
        if self.tasks.contains_key(&task.id) {
            return false;
        }
        let id = task.id.clone();
        let idx = self.task_index.len();
        self.task_index.insert(id.clone(), idx);
        self.tasks.insert(id, task);
        true
    }

    /// Remove a task from the graph.
    ///
    /// Also removes any edges connected to this task.
    /// Returns the removed task if it existed.
    pub fn remove_task(&mut self, id: &str) -> Option<Task> {
        let task = self.tasks.remove(id)?;
        self.task_index.remove(id);

        // Remove all edges involving this task
        self.edges.retain(|e| e.from != id && e.to != id);

        // Rebuild index since positions changed
        self.rebuild_index();

        Some(task)
    }

    /// Get a task by ID.
    pub fn get_task(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }

    /// Get a mutable reference to a task by ID.
    pub fn get_task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.get_mut(id)
    }

    /// Add a dependency edge to the graph.
    ///
    /// Returns `false` if either task doesn't exist or the edge already exists.
    pub fn add_edge(&mut self, edge: Edge) -> bool {
        if !self.tasks.contains_key(&edge.from) || !self.tasks.contains_key(&edge.to) {
            return false;
        }

        // Check for duplicate edge
        if self
            .edges
            .iter()
            .any(|e| e.from == edge.from && e.to == edge.to && e.dep_type == edge.dep_type)
        {
            return false;
        }

        self.edges.push(edge);
        true
    }

    /// Remove an edge from the graph.
    ///
    /// Returns `true` if the edge was found and removed.
    pub fn remove_edge(&mut self, from: &str, to: &str, dep_type: DependencyType) -> bool {
        let original_len = self.edges.len();
        self.edges
            .retain(|e| !(e.from == from && e.to == to && e.dep_type == dep_type));
        self.edges.len() < original_len
    }

    /// Get all tasks in the graph.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values()
    }

    /// Get all tasks mutably.
    pub fn tasks_mut(&mut self) -> impl Iterator<Item = &mut Task> {
        self.tasks.values_mut()
    }

    /// Get all edges in the graph.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Get the number of tasks in the graph.
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    /// Get the number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Get all tasks matching a predicate.
    pub fn filter_tasks<F>(&self, predicate: F) -> Vec<&Task>
    where
        F: Fn(&Task) -> bool,
    {
        self.tasks.values().filter(|t| predicate(t)).collect()
    }

    /// Clear all tasks and edges from the graph.
    pub fn clear(&mut self) {
        self.tasks.clear();
        self.edges.clear();
        self.task_index.clear();
    }

    /// Rebuild the task index after structural changes.
    fn rebuild_index(&mut self) {
        self.task_index.clear();
        for (idx, id) in self.tasks.keys().enumerate() {
            self.task_index.insert(id.clone(), idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_graph_add_and_remove_tasks() {
        let mut graph = TaskGraph::new();
        let task = Task::new("t1".into(), "Test task".into(), "Description".into());
        assert!(graph.add_task(task));
        assert!(graph.get_task("t1").is_some());
        assert!(!graph.add_task(Task::new("t1".into(), "Duplicate".into(), "".into())));
        assert_eq!(graph.task_count(), 1);

        let removed = graph.remove_task("t1");
        assert!(removed.is_some());
        assert_eq!(graph.task_count(), 0);
    }

    #[test]
    fn task_graph_add_and_remove_edges() {
        let mut graph = TaskGraph::new();
        graph.add_task(Task::new("a".into(), "A".into(), "".into()));
        graph.add_task(Task::new("b".into(), "B".into(), "".into()));

        let edge = Edge {
            from: "a".into(),
            to: "b".into(),
            dep_type: DependencyType::Blocks,
        };
        assert!(graph.add_edge(edge));
        assert_eq!(graph.edge_count(), 1);

        // Duplicate edge should fail
        assert!(!graph.add_edge(Edge {
            from: "a".into(),
            to: "b".into(),
            dep_type: DependencyType::Blocks,
        }));

        // Remove edge
        assert!(graph.remove_edge("a", "b", DependencyType::Blocks));
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn task_graph_edge_removal_on_task_removal() {
        let mut graph = TaskGraph::new();
        graph.add_task(Task::new("a".into(), "A".into(), "".into()));
        graph.add_task(Task::new("b".into(), "B".into(), "".into()));
        graph.add_task(Task::new("c".into(), "C".into(), "".into()));

        graph.add_edge(Edge {
            from: "a".into(),
            to: "b".into(),
            dep_type: DependencyType::Blocks,
        });
        graph.add_edge(Edge {
            from: "b".into(),
            to: "c".into(),
            dep_type: DependencyType::Blocks,
        });

        assert_eq!(graph.edge_count(), 2);

        // Removing 'b' should remove both edges
        graph.remove_task("b");
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn task_state_checks() {
        let mut task = Task::new("t1".into(), "Test".into(), "".into());
        assert!(task.is_ready());
        assert!(!task.is_terminal());
        assert!(!task.is_blocked());
        assert!(!task.is_leased());

        use crate::reliability::state::TerminalState;
        use chrono::Utc;

        task.state = RuntimeState::Terminal(TerminalState::Succeeded {
            patch_digest: "abc".into(),
            verify_run_id: "v1".into(),
            completed_at: Utc::now(),
        });
        assert!(task.is_terminal());
        assert!(!task.is_ready());
    }
}
