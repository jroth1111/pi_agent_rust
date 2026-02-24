//! DAG operations for the task graph.
//!
//! This module provides algorithms for working with the task graph as a
//! directed acyclic graph (DAG), including:
//!
//! - Cycle detection
//! - Topological sorting
//! - Dependency queries (transitive)
//! - Unblock detection

use crate::error::{Error, Result};
use crate::reliability::state::FailureClass;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

use super::{TaskGraph, TaskId};

/// Kinds of dependencies between tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum DependencyType {
    /// Hard prerequisite - `to` cannot start until `from` is complete.
    #[default]
    Blocks,
    /// Hierarchy - `to` is a child of `from` (children are part of parent).
    ParentChild,
    /// Conditional - `to` runs only if `from` fails with a matching class.
    ConditionalBlocks,
    /// Fanout gate - `to` waits for `from` to reach a certain state.
    WaitsFor,
}

/// An edge in the task graph representing a dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// Source task ID (the task that blocks).
    pub from: TaskId,
    /// Target task ID (the task being blocked).
    pub to: TaskId,
    /// Type of dependency relationship.
    pub dep_type: DependencyType,
}

/// Represents a cycle detected in the task graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle(pub Vec<TaskId>);

impl std::fmt::Display for Cycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cycle: ")?;
        for (i, id) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "{id}")?;
        }
        write!(
            f,
            " -> {}",
            self.0.first().map_or("", std::string::String::as_str)
        )
    }
}

/// Trigger for unblocking tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Task completed successfully.
    Completed,
    /// Task failed with specific failure classes.
    Failed { class: FailureClassMask },
}

/// Bitflags for failure class matching.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureClassMask {
    /// The failure classes this mask matches.
    pub classes: Vec<FailureClass>,
}

impl FailureClassMask {
    /// Create a new empty mask.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a mask from a list of failure classes.
    pub const fn from_classes(classes: Vec<FailureClass>) -> Self {
        Self { classes }
    }

    /// Check if a failure class matches this mask.
    pub fn matches(&self, class: FailureClass) -> bool {
        self.classes.contains(&class)
    }

    /// Add a failure class to the mask.
    pub fn add(&mut self, class: FailureClass) {
        if !self.classes.contains(&class) {
            self.classes.push(class);
        }
    }
}

impl TaskGraph {
    /// Detect cycles in the dependency graph.
    ///
    /// Returns a list of all cycles found. An empty vector means the graph is acyclic.
    /// Only considers `Blocks` and `ConditionalBlocks` edges for cycle detection.
    pub fn detect_cycles(&self) -> Vec<Cycle> {
        let mut cycles = Vec::new();
        let adjacency = self.build_adjacency_for_cycles();

        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();
        let mut path = Vec::new();

        for node in self.tasks.keys() {
            let node = node.as_str();
            if !visited.contains(node) {
                self.dfs_find_cycles(
                    node,
                    &adjacency,
                    &mut visited,
                    &mut rec_stack,
                    &mut path,
                    &mut cycles,
                );
            }
        }

        cycles
    }

    /// Check if the graph has any cycles.
    pub fn has_cycles(&self) -> bool {
        self.detect_cycles().iter().any(|c| !c.0.is_empty())
    }

    /// Perform a topological sort of the tasks.
    ///
    /// Returns an error if cycles exist in the graph.
    /// Only considers `Blocks` edges for ordering.
    pub fn topological_sort(&self) -> Result<Vec<TaskId>> {
        if self.has_cycles() {
            return Err(Error::Validation(
                "Cannot topologically sort a graph with cycles".into(),
            ));
        }

        // Kahn's algorithm
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

        // Initialize
        for task_id in self.tasks.keys() {
            in_degree.insert(task_id.as_str(), 0);
            adjacency.insert(task_id.as_str(), Vec::new());
        }

        // Build adjacency (from -> to means 'from' must come before 'to')
        for edge in &self.edges {
            if edge.dep_type == DependencyType::Blocks {
                adjacency
                    .get_mut(edge.from.as_str())
                    .unwrap()
                    .push(&edge.to);
                *in_degree.get_mut(edge.to.as_str()).unwrap() += 1;
            }
        }

        // Queue of nodes with no incoming edges
        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(id, _)| *id)
            .collect();

        let mut result = Vec::with_capacity(self.tasks.len());

        while let Some(node) = queue.pop_front() {
            result.push(node.to_string());

            if let Some(neighbors) = adjacency.get(node) {
                for &neighbor in neighbors {
                    let degree = in_degree.get_mut(neighbor).unwrap();
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(neighbor);
                    }
                }
            }
        }

        Ok(result)
    }

    /// Get tasks that are newly unblocked after a task closes.
    ///
    /// This finds all tasks that were blocked by the closed task and are now
    /// ready to run because all their blocking dependencies are satisfied.
    pub fn newly_unblocked(&self, closed_id: &TaskId, trigger: &Trigger) -> Vec<TaskId> {
        let mut unblocked = Vec::new();

        // Find tasks that have the closed task as a blocker
        for edge in &self.edges {
            if &edge.from != closed_id {
                continue;
            }

            // Check if this edge type is satisfied by the trigger
            let satisfied = match (&edge.dep_type, trigger) {
                (DependencyType::Blocks, Trigger::Completed) => true,
                (DependencyType::ConditionalBlocks, Trigger::Failed { class }) => {
                    // The edge is satisfied if the failure class matches
                    class.classes.contains(&FailureClass::VerificationFailed)
                        || class.classes.contains(&FailureClass::ScopeCreepDetected)
                }
                (DependencyType::WaitsFor, Trigger::Completed) => true,
                _ => false,
            };

            if !satisfied {
                continue;
            }

            // Check if all blockers for this task are now satisfied
            let target_id = &edge.to;
            if self.all_blockers_satisfied(target_id) {
                unblocked.push(target_id.clone());
            }
        }

        unblocked.sort();
        unblocked.dedup();
        unblocked
    }

    /// Get all dependencies of a task (transitive closure).
    ///
    /// Returns all tasks that must complete before the given task can start,
    /// including indirect dependencies through other tasks.
    pub fn dependencies(&self, task_id: &TaskId) -> Vec<TaskId> {
        let mut deps = Vec::new();
        let mut visited = HashSet::new();
        self.collect_dependencies(task_id, &mut deps, &mut visited);
        deps
    }

    /// Get all dependents of a task (transitive closure).
    ///
    /// Returns all tasks that depend on the given task, directly or indirectly.
    pub fn dependents(&self, task_id: &TaskId) -> Vec<TaskId> {
        let mut deps = Vec::new();
        let mut visited = HashSet::new();
        self.collect_dependents(task_id, &mut deps, &mut visited);
        deps
    }

    /// Get direct dependencies of a task.
    pub fn direct_dependencies(&self, task_id: &TaskId) -> Vec<TaskId> {
        self.edges
            .iter()
            .filter(|e| {
                &e.to == task_id
                    && matches!(
                        e.dep_type,
                        DependencyType::Blocks | DependencyType::ConditionalBlocks
                    )
            })
            .map(|e| e.from.clone())
            .collect()
    }

    /// Get direct dependents of a task.
    pub fn direct_dependents(&self, task_id: &TaskId) -> Vec<TaskId> {
        self.edges
            .iter()
            .filter(|e| {
                &e.from == task_id
                    && matches!(
                        e.dep_type,
                        DependencyType::Blocks | DependencyType::ConditionalBlocks
                    )
            })
            .map(|e| e.to.clone())
            .collect()
    }

    /// Check if a task has any dependencies.
    pub fn has_dependencies(&self, task_id: &TaskId) -> bool {
        self.edges
            .iter()
            .any(|e| &e.to == task_id && matches!(e.dep_type, DependencyType::Blocks))
    }

    /// Get all tasks that have no dependencies (roots of the DAG).
    pub fn roots(&self) -> Vec<TaskId> {
        self.tasks
            .keys()
            .filter(|id| !self.has_dependencies(id))
            .cloned()
            .collect()
    }

    /// Get all tasks that have no dependents (leaves of the DAG).
    pub fn leaves(&self) -> Vec<TaskId> {
        let has_dependents: HashSet<&str> = self
            .edges
            .iter()
            .filter(|e| matches!(e.dep_type, DependencyType::Blocks))
            .map(|e| e.from.as_str())
            .collect();

        self.tasks
            .keys()
            .filter(|id| !has_dependents.contains(id.as_str()))
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // Private helper methods
    // -----------------------------------------------------------------------

    fn build_adjacency_for_cycles(&self) -> HashMap<&str, Vec<&str>> {
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

        for edge in &self.edges {
            if matches!(
                edge.dep_type,
                DependencyType::Blocks | DependencyType::ConditionalBlocks
            ) {
                adjacency
                    .entry(edge.from.as_str())
                    .or_default()
                    .push(edge.to.as_str());
            }
        }

        adjacency
    }

    fn dfs_find_cycles<'a>(
        &'a self,
        node: &'a str,
        adjacency: &HashMap<&'a str, Vec<&'a str>>,
        visited: &mut HashSet<&'a str>,
        rec_stack: &mut HashSet<&'a str>,
        path: &mut Vec<&'a str>,
        cycles: &mut Vec<Cycle>,
    ) {
        visited.insert(node);
        rec_stack.insert(node);
        path.push(node);

        if let Some(neighbors) = adjacency.get(node) {
            for &neighbor in neighbors {
                if !visited.contains(neighbor) {
                    self.dfs_find_cycles(neighbor, adjacency, visited, rec_stack, path, cycles);
                } else if rec_stack.contains(neighbor) {
                    // Found a cycle - extract it from the path
                    let cycle_start = path.iter().position(|&n| n == neighbor).unwrap_or(0);
                    let cycle_nodes: Vec<TaskId> = path[cycle_start..]
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect();
                    cycles.push(Cycle(cycle_nodes));
                }
            }
        }

        path.pop();
        rec_stack.remove(node);
    }

    fn collect_dependencies(
        &self,
        task_id: &TaskId,
        deps: &mut Vec<TaskId>,
        visited: &mut HashSet<TaskId>,
    ) {
        if visited.contains(task_id) {
            return;
        }
        visited.insert(task_id.clone());

        for edge in &self.edges {
            if &edge.to == task_id
                && matches!(
                    edge.dep_type,
                    DependencyType::Blocks | DependencyType::ConditionalBlocks
                )
            {
                deps.push(edge.from.clone());
                self.collect_dependencies(&edge.from, deps, visited);
            }
        }
    }

    fn collect_dependents(
        &self,
        task_id: &TaskId,
        deps: &mut Vec<TaskId>,
        visited: &mut HashSet<TaskId>,
    ) {
        if visited.contains(task_id) {
            return;
        }
        visited.insert(task_id.clone());

        for edge in &self.edges {
            if &edge.from == task_id
                && matches!(
                    edge.dep_type,
                    DependencyType::Blocks | DependencyType::ConditionalBlocks
                )
            {
                deps.push(edge.to.clone());
                self.collect_dependents(&edge.to, deps, visited);
            }
        }
    }

    fn all_blockers_satisfied(&self, task_id: &TaskId) -> bool {
        for edge in &self.edges {
            if &edge.to != task_id {
                continue;
            }

            if !matches!(
                edge.dep_type,
                DependencyType::Blocks | DependencyType::ConditionalBlocks
            ) {
                continue;
            }

            // Check if the blocker is in a terminal state
            let Some(blocker) = self.get_task(&edge.from) else {
                continue;
            };

            if !blocker.is_terminal() {
                return false;
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_graph::Task;

    fn make_graph_with_edges(edges: Vec<(&str, &str, DependencyType)>) -> TaskGraph {
        let mut graph = TaskGraph::new();

        // Collect all unique task IDs
        let mut task_ids = HashSet::new();
        for (from, to, _) in &edges {
            task_ids.insert(*from);
            task_ids.insert(*to);
        }

        for id in task_ids {
            graph.add_task(Task::new(id.into(), format!("Task {id}"), "".into()));
        }

        for (from, to, dep_type) in edges {
            graph.add_edge(Edge {
                from: from.into(),
                to: to.into(),
                dep_type,
            });
        }

        graph
    }

    #[test]
    fn detect_no_cycle() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("b", "c", DependencyType::Blocks),
        ]);

        let cycles = graph.detect_cycles();
        assert!(cycles.is_empty());
        assert!(!graph.has_cycles());
    }

    #[test]
    fn detect_simple_cycle() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("b", "a", DependencyType::Blocks),
        ]);

        let cycles = graph.detect_cycles();
        assert!(!cycles.is_empty());
        assert!(graph.has_cycles());
    }

    #[test]
    fn detect_longer_cycle() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("b", "c", DependencyType::Blocks),
            ("c", "a", DependencyType::Blocks),
        ]);

        assert!(graph.has_cycles());
    }

    #[test]
    fn topological_sort_no_cycles() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("a", "c", DependencyType::Blocks),
            ("b", "d", DependencyType::Blocks),
            ("c", "d", DependencyType::Blocks),
        ]);

        let sorted = graph.topological_sort().expect("should succeed");
        assert_eq!(sorted.len(), 4);

        // 'a' must come before 'b', 'c', and 'd'
        let a_pos = sorted.iter().position(|id| id == "a").unwrap();
        let b_pos = sorted.iter().position(|id| id == "b").unwrap();
        let c_pos = sorted.iter().position(|id| id == "c").unwrap();
        let d_pos = sorted.iter().position(|id| id == "d").unwrap();

        assert!(a_pos < b_pos);
        assert!(a_pos < c_pos);
        assert!(b_pos < d_pos);
        assert!(c_pos < d_pos);
    }

    #[test]
    fn topological_sort_with_cycles_fails() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("b", "a", DependencyType::Blocks),
        ]);

        assert!(graph.topological_sort().is_err());
    }

    #[test]
    fn dependencies_transitive() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("b", "c", DependencyType::Blocks),
            ("c", "d", DependencyType::Blocks),
        ]);

        let deps = graph.dependencies(&"d".into());
        assert_eq!(deps.len(), 3);
        assert!(deps.contains(&"a".to_string()));
        assert!(deps.contains(&"b".to_string()));
        assert!(deps.contains(&"c".to_string()));
    }

    #[test]
    fn dependents_transitive() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("b", "c", DependencyType::Blocks),
            ("c", "d", DependencyType::Blocks),
        ]);

        let deps = graph.dependents(&"a".into());
        assert_eq!(deps.len(), 3);
        assert!(deps.contains(&"b".to_string()));
        assert!(deps.contains(&"c".to_string()));
        assert!(deps.contains(&"d".to_string()));
    }

    #[test]
    fn roots_and_leaves() {
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("a", "c", DependencyType::Blocks),
            ("b", "d", DependencyType::Blocks),
            ("c", "d", DependencyType::Blocks),
        ]);

        let roots = graph.roots();
        assert_eq!(roots, vec!["a".to_string()]);

        let leaves = graph.leaves();
        assert_eq!(leaves, vec!["d".to_string()]);
    }

    #[test]
    fn newly_unblocked_after_close() {
        use crate::reliability::state::{RuntimeState, TerminalState};
        use chrono::Utc;

        let mut graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::Blocks),
            ("b", "c", DependencyType::Blocks),
        ]);

        // Mark 'a' as completed
        let task_a = graph.get_task_mut("a").unwrap();
        task_a.state = RuntimeState::Terminal(TerminalState::Succeeded {
            patch_digest: "x".into(),
            verify_run_id: "v1".into(),
            completed_at: Utc::now(),
        });

        let unblocked = graph.newly_unblocked(&"a".into(), &Trigger::Completed);
        assert_eq!(unblocked, vec!["b".to_string()]);
    }

    #[test]
    fn parent_child_not_in_cycle_detection() {
        // Parent-child relationships shouldn't contribute to cycles
        let graph = make_graph_with_edges(vec![
            ("a", "b", DependencyType::ParentChild),
            ("b", "a", DependencyType::ParentChild),
        ]);

        assert!(!graph.has_cycles());
    }
}
