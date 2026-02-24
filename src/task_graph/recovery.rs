//! Lease expiration and recovery utilities for the task graph.
//!
//! This module provides functionality for:
//!
//! - Detecting and expiring stale leases
//! - Agent liveness tracking
//! - Deferred task management (time-based and dependency-based)
//!
//! ## Lease Expiration
//!
//! When an agent holds a lease on a task but doesn't complete it within
//! the timeout period, the lease should be expired and the task returned
//! to the Ready state so another agent can pick it up.
//!
//! ## Deferred Tasks
//!
//! Tasks can be deferred for two reasons:
//! 1. Time-based: Resume after a specific timestamp
//! 2. Dependency-based: Resume when a blocking task completes

use crate::reliability::state::{DeferTrigger, RuntimeState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::{Task, TaskId};

/// Agent state for liveness tracking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentState {
    /// Agent is idle and available for work.
    Idle,
    /// Agent is actively working on a task.
    Running {
        /// Unix timestamp of last activity.
        last_activity: u64,
    },
    /// Agent is blocked and needs help.
    Stuck {
        /// Reason for being stuck.
        reason: String,
    },
    /// Agent has stopped gracefully.
    Stopped {
        /// Timestamp when the agent stopped.
        stopped_at: u64,
    },
    /// Agent died without clean shutdown.
    Dead {
        /// Timestamp when death was detected.
        died_at: u64,
    },
}

impl AgentState {
    /// Create a new idle agent state.
    pub const fn idle() -> Self {
        Self::Idle
    }

    /// Create a running state with the current timestamp.
    pub fn running_now() -> Self {
        Self::Running {
            last_activity: current_timestamp(),
        }
    }

    /// Check if this agent is available for work.
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Idle | Self::Running { .. })
    }

    /// Check if this agent is alive (not dead or stopped).
    pub const fn is_alive(&self) -> bool {
        !matches!(self, Self::Dead { .. } | Self::Stopped { .. })
    }

    /// Update the last activity timestamp if running.
    pub fn touch(&mut self) {
        if let Self::Running { last_activity } = self {
            *last_activity = current_timestamp();
        }
    }
}

/// Deferred task with time-based or dependency-based resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredTask {
    /// ID of the deferred task.
    pub task_id: TaskId,
    /// Time when the task should be resumed (if time-based).
    pub defer_until: Option<u64>,
    /// Task that this is waiting on (if dependency-based).
    pub blocked_by: Option<TaskId>,
    /// Reason for deferral.
    pub reason: String,
}

impl DeferredTask {
    /// Create a time-based deferred task.
    pub const fn until_time(task_id: TaskId, until: u64, reason: String) -> Self {
        Self {
            task_id,
            defer_until: Some(until),
            blocked_by: None,
            reason,
        }
    }

    /// Create a dependency-based deferred task.
    pub const fn until_dependency(task_id: TaskId, blocker: TaskId, reason: String) -> Self {
        Self {
            task_id,
            defer_until: None,
            blocked_by: Some(blocker),
            reason,
        }
    }

    /// Check if this task is ready to be resumed.
    pub fn is_ready(&self, now: u64, completed_tasks: &std::collections::HashSet<&str>) -> bool {
        // Time-based: check if we've passed the defer time
        if let Some(until) = self.defer_until {
            if now < until {
                return false;
            }
        }

        // Dependency-based: check if the blocker is complete
        if let Some(ref blocker) = self.blocked_by {
            if !completed_tasks.contains(blocker.as_str()) {
                return false;
            }
        }

        true
    }
}

/// Expire stale leases and return stuck tasks to Ready state.
///
/// This function scans all tasks for leased tasks whose lease has expired,
/// and returns them to the Ready state so they can be claimed by other agents.
///
/// # Arguments
///
/// * `tasks` - Mutable reference to the task map
/// * `lease_timeout_secs` - Number of seconds after which a lease is considered stale
/// * `now` - Current Unix timestamp
///
/// # Returns
///
/// A vector of task IDs whose leases were expired.
pub fn expire_stale_leases(
    tasks: &mut HashMap<TaskId, Task>,
    lease_timeout_secs: u64,
    now: u64,
) -> Vec<TaskId> {
    let mut expired = Vec::new();

    for (id, task) in tasks.iter_mut() {
        if let RuntimeState::Leased { expires_at, .. } = &task.state {
            // Convert DateTime<Utc> to Unix timestamp for comparison
            let expires_ts = expires_at.timestamp() as u64;
            if now > expires_ts.saturating_add(lease_timeout_secs) {
                // Reset to Ready state
                task.state = RuntimeState::Ready;
                task.owner = None;
                expired.push(id.clone());
            }
        }
    }

    expired
}

/// Check deferred tasks and return those ready to be resumed.
///
/// # Arguments
///
/// * `deferred` - List of deferred tasks to check
/// * `now` - Current Unix timestamp
/// * `completed_tasks` - Set of task IDs that have completed
///
/// # Returns
///
/// A vector of task IDs ready to be resumed.
pub fn check_deferred_tasks(
    deferred: &[DeferredTask],
    now: u64,
    completed_tasks: &std::collections::HashSet<&str>,
) -> Vec<TaskId> {
    deferred
        .iter()
        .filter(|d| d.is_ready(now, completed_tasks))
        .map(|d| d.task_id.clone())
        .collect()
}

/// Convert a DeferTrigger to a DeferredTask.
pub fn defer_trigger_to_task(
    task_id: TaskId,
    trigger: &DeferTrigger,
    reason: String,
) -> DeferredTask {
    match trigger {
        DeferTrigger::Until(datetime) => {
            let ts = datetime.timestamp() as u64;
            DeferredTask::until_time(task_id, ts, reason)
        }
        DeferTrigger::DependsOn(blocker) => {
            DeferredTask::until_dependency(task_id, blocker.clone(), reason)
        }
    }
}

/// Agent registry for tracking agent liveness.
#[derive(Debug, Default)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentState>,
    /// Heartbeat timeout in seconds.
    heartbeat_timeout_secs: u64,
}

impl AgentRegistry {
    /// Create a new agent registry with the specified heartbeat timeout.
    pub fn new(heartbeat_timeout_secs: u64) -> Self {
        Self {
            agents: HashMap::new(),
            heartbeat_timeout_secs,
        }
    }

    /// Register a new agent.
    pub fn register(&mut self, agent_id: String) {
        self.agents.insert(agent_id, AgentState::idle());
    }

    /// Unregister an agent.
    pub fn unregister(&mut self, agent_id: &str) {
        self.agents.remove(agent_id);
    }

    /// Update agent state.
    pub fn update(&mut self, agent_id: &str, state: AgentState) {
        self.agents.insert(agent_id.to_string(), state);
    }

    /// Record a heartbeat from an agent.
    pub fn heartbeat(&mut self, agent_id: &str) {
        if let Some(state) = self.agents.get_mut(agent_id) {
            state.touch();
        }
    }

    /// Get the state of an agent.
    pub fn get(&self, agent_id: &str) -> Option<&AgentState> {
        self.agents.get(agent_id)
    }

    /// Check for dead agents (missed heartbeats).
    pub fn detect_dead_agents(&mut self, now: u64) -> Vec<String> {
        let timeout = self.heartbeat_timeout_secs;
        let mut dead = Vec::new();

        for (id, state) in &mut self.agents {
            if let AgentState::Running { last_activity } = state {
                if now > last_activity.saturating_add(timeout) {
                    *state = AgentState::Dead { died_at: now };
                    dead.push(id.clone());
                }
            }
        }

        dead
    }

    /// Get all alive agents.
    pub fn alive_agents(&self) -> Vec<&String> {
        self.agents
            .iter()
            .filter(|(_, state)| state.is_alive())
            .map(|(id, _)| id)
            .collect()
    }

    /// Get all available agents (idle or running).
    pub fn available_agents(&self) -> Vec<&String> {
        self.agents
            .iter()
            .filter(|(_, state)| state.is_available())
            .map(|(id, _)| id)
            .collect()
    }

    /// Get count of registered agents.
    pub fn count(&self) -> usize {
        self.agents.len()
    }
}

/// Get the current Unix timestamp.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::state::RuntimeState;
    use chrono::{Duration, Utc};

    fn leased_task(id: &str, expires_at: chrono::DateTime<Utc>) -> Task {
        let mut task = Task::new(id.into(), "Test".into(), "".into());
        task.state = RuntimeState::Leased {
            lease_id: "lease-1".into(),
            agent_id: "agent-1".into(),
            fence_token: 1,
            expires_at,
        };
        task.owner = Some("agent-1".into());
        task
    }

    #[test]
    fn expire_stale_leases_basic() {
        let mut tasks = HashMap::new();
        let now = Utc::now();

        // Expired lease (expired 100 seconds ago)
        tasks.insert("t1".into(), leased_task("t1", now - Duration::seconds(100)));

        // Active lease
        tasks.insert(
            "t2".into(),
            leased_task("t2", now + Duration::seconds(3600)),
        );

        let current_ts = now.timestamp() as u64;
        let expired = expire_stale_leases(&mut tasks, 60, current_ts);

        assert_eq!(expired, vec!["t1".to_string()]);
        assert!(tasks.get("t1").unwrap().is_ready());
        assert!(tasks.get("t2").unwrap().is_leased());
    }

    #[test]
    fn agent_state_checks() {
        let mut state = AgentState::running_now();
        assert!(state.is_available());
        assert!(state.is_alive());

        state = AgentState::Dead {
            died_at: current_timestamp(),
        };
        assert!(!state.is_available());
        assert!(!state.is_alive());

        state = AgentState::Idle;
        assert!(state.is_available());
        assert!(state.is_alive());
    }

    #[test]
    fn agent_registry_basic() {
        let mut registry = AgentRegistry::new(300);
        registry.register("agent-1".into());
        registry.register("agent-2".into());

        assert_eq!(registry.count(), 2);

        registry.heartbeat("agent-1");
        assert!(registry.get("agent-1").unwrap().is_alive());

        registry.unregister("agent-1");
        assert_eq!(registry.count(), 1);
    }

    #[test]
    fn agent_registry_detect_dead() {
        let mut registry = AgentRegistry::new(60); // 60 second timeout

        // Manually insert a running agent with old timestamp
        registry.agents.insert(
            "dead-agent".into(),
            AgentState::Running {
                last_activity: current_timestamp() - 120,
            },
        );
        registry.agents.insert(
            "alive-agent".into(),
            AgentState::Running {
                last_activity: current_timestamp(),
            },
        );

        let dead = registry.detect_dead_agents(current_timestamp());
        assert_eq!(dead, vec!["dead-agent".to_string()]);
        assert!(matches!(
            registry.get("dead-agent"),
            Some(AgentState::Dead { .. })
        ));
    }

    #[test]
    fn deferred_task_time_based() {
        let now = current_timestamp();
        let deferred =
            DeferredTask::until_time("t1".into(), now + 100, "Waiting for cooldown".into());

        let completed = std::collections::HashSet::new();

        // Not ready yet
        assert!(!deferred.is_ready(now, &completed));

        // Ready after time passes
        assert!(deferred.is_ready(now + 200, &completed));
    }

    #[test]
    fn deferred_task_dependency_based() {
        let deferred = DeferredTask::until_dependency(
            "t1".into(),
            "blocker".into(),
            "Waiting for blocker".into(),
        );

        let now = current_timestamp();

        let mut completed = std::collections::HashSet::new();
        assert!(!deferred.is_ready(now, &completed));

        completed.insert("blocker");
        assert!(deferred.is_ready(now, &completed));
    }

    #[test]
    fn check_deferred_tasks_batch() {
        let now = current_timestamp();
        let deferred = vec![
            DeferredTask::until_time("t1".into(), now + 100, "Not ready".into()),
            DeferredTask::until_time("t2".into(), now - 10, "Ready".into()),
            DeferredTask::until_dependency("t3".into(), "blocker".into(), "Waiting".into()),
        ];

        let mut completed = std::collections::HashSet::new();
        completed.insert("blocker");

        let ready = check_deferred_tasks(&deferred, now, &completed);
        assert_eq!(ready, vec!["t2".to_string(), "t3".to_string()]);
    }
}
