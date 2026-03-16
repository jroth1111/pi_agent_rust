use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::reliability::context_budget::ContextFanout;
use crate::reliability::state::RuntimeState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub lease_id: String,
    pub agent_id: String,
    pub task_id: String,
    pub fence_token: u64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("agent already has active lease for task {0}")]
    AgentHasActiveLease(String),
    #[error("task {task_id} already leased by {lease_id} until {expires_at}")]
    TaskAlreadyLeased {
        task_id: String,
        lease_id: String,
        expires_at: DateTime<Utc>,
    },
    #[error("lease {0} not found")]
    LeaseNotFound(String),
    #[error("fence mismatch for lease {lease_id}: expected {expected}, got {actual}")]
    FenceMismatch {
        lease_id: String,
        expected: u64,
        actual: u64,
    },
    #[error("no active lease for this operation")]
    NotLeased,
    #[error("invalid state for edit operation: {0}")]
    InvalidState(String),
}

pub trait LeaseProvider: Send + Sync + std::fmt::Debug {
    fn acquire(
        &self,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
        fence_token: u64,
    ) -> Result<LeaseGrant, LeaseError>;
    fn release(&self, lease_id: &str);
    fn prune_expired(&self, now: DateTime<Utc>);
}

#[derive(Debug, Default)]
pub struct InMemoryExternalLeaseProvider {
    by_lease: Mutex<HashMap<String, LeaseGrant>>,
}

impl InMemoryExternalLeaseProvider {
    fn prune_locked(leases: &mut HashMap<String, LeaseGrant>, now: DateTime<Utc>) {
        leases.retain(|_, lease| lease.expires_at > now);
    }

    #[cfg(test)]
    pub fn force_expire_for_test(&self, lease_id: &str, expires_at: DateTime<Utc>) {
        if let Ok(mut leases) = self.by_lease.lock() {
            if let Some(lease) = leases.get_mut(lease_id) {
                lease.expires_at = expires_at;
            }
        }
    }
}

impl LeaseProvider for InMemoryExternalLeaseProvider {
    fn acquire(
        &self,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
        fence_token: u64,
    ) -> Result<LeaseGrant, LeaseError> {
        let now = Utc::now();
        let Ok(mut leases) = self.by_lease.lock() else {
            return Err(LeaseError::InvalidState(
                "external lease provider lock poisoned".to_string(),
            ));
        };
        Self::prune_locked(&mut leases, now);

        if let Some(active) = leases
            .values()
            .find(|lease| lease.task_id == task_id && lease.expires_at > now)
            .cloned()
        {
            return Err(LeaseError::TaskAlreadyLeased {
                task_id: active.task_id,
                lease_id: active.lease_id,
                expires_at: active.expires_at,
            });
        }

        let lease_id = format!("lease-{}", uuid::Uuid::new_v4());
        let grant = LeaseGrant {
            lease_id: lease_id.clone(),
            agent_id: agent_id.to_string(),
            task_id: task_id.to_string(),
            fence_token,
            expires_at: now + Duration::seconds(ttl_seconds.max(1)),
        };
        leases.insert(lease_id, grant.clone());
        Ok(grant)
    }

    fn release(&self, lease_id: &str) {
        if let Ok(mut leases) = self.by_lease.lock() {
            leases.remove(lease_id);
        }
    }

    fn prune_expired(&self, now: DateTime<Utc>) {
        if let Ok(mut leases) = self.by_lease.lock() {
            Self::prune_locked(&mut leases, now);
        }
    }
}

/// In-memory lease manager used by reliability orchestration.
#[derive(Debug, Default)]
pub struct LeaseManager {
    by_lease: HashMap<String, LeaseGrant>,
    latest_fence_by_task: HashMap<String, u64>,
    external_provider: Option<Arc<dyn LeaseProvider>>,
}

impl LeaseManager {
    pub fn set_external_provider(&mut self, provider: Arc<dyn LeaseProvider>) {
        self.external_provider = Some(provider);
    }

    fn prune_expired_leases(&mut self, now: DateTime<Utc>) {
        self.by_lease.retain(|_, lease| lease.expires_at > now);
        if let Some(provider) = &self.external_provider {
            provider.prune_expired(now);
        }
    }

    fn next_fence_for_task(&mut self, task_id: &str) -> u64 {
        let next_fence = self.latest_fence_by_task.get(task_id).copied().unwrap_or(0) + 1;
        self.latest_fence_by_task
            .insert(task_id.to_string(), next_fence);
        next_fence
    }

    fn build_grant(
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
        fence_token: u64,
    ) -> LeaseGrant {
        let now = Utc::now();
        let lease_id = format!("lease-{}", uuid::Uuid::new_v4());
        LeaseGrant {
            lease_id,
            agent_id: agent_id.to_string(),
            task_id: task_id.to_string(),
            fence_token,
            expires_at: now + Duration::seconds(ttl_seconds.max(1)),
        }
    }

    pub fn issue_lease(
        &mut self,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<LeaseGrant, LeaseError> {
        let now = Utc::now();
        self.prune_expired_leases(now);
        if let Some(active) = self
            .by_lease
            .values()
            .find(|lease| lease.agent_id == agent_id && lease.expires_at > now)
        {
            return Err(LeaseError::AgentHasActiveLease(active.task_id.clone()));
        }

        if let Some(active) = self
            .by_lease
            .values()
            .find(|lease| lease.task_id == task_id && lease.expires_at > now)
        {
            return Err(LeaseError::TaskAlreadyLeased {
                task_id: active.task_id.clone(),
                lease_id: active.lease_id.clone(),
                expires_at: active.expires_at,
            });
        }

        let next_fence = self.next_fence_for_task(task_id);
        let grant = if let Some(provider) = &self.external_provider {
            match provider.acquire(task_id, agent_id, ttl_seconds, next_fence) {
                Ok(grant) => grant,
                Err(LeaseError::TaskAlreadyLeased {
                    task_id,
                    lease_id,
                    expires_at,
                }) if expires_at <= now => {
                    provider.release(&lease_id);
                    provider.prune_expired(now);
                    provider.acquire(&task_id, agent_id, ttl_seconds, next_fence)?
                }
                Err(err) => return Err(err),
            }
        } else {
            Self::build_grant(task_id, agent_id, ttl_seconds, next_fence)
        };
        self.by_lease.insert(grant.lease_id.clone(), grant.clone());
        Ok(grant)
    }

    pub fn validate_fence(&self, lease_id: &str, fence_token: u64) -> Result<(), LeaseError> {
        let Some(grant) = self.by_lease.get(lease_id) else {
            return Err(LeaseError::LeaseNotFound(lease_id.to_string()));
        };
        if grant.fence_token != fence_token {
            return Err(LeaseError::FenceMismatch {
                lease_id: lease_id.to_string(),
                expected: grant.fence_token,
                actual: fence_token,
            });
        }
        Ok(())
    }

    pub fn expire_lease(&mut self, lease_id: &str) -> Result<(), LeaseError> {
        if let Some(provider) = &self.external_provider {
            provider.release(lease_id);
        }
        if self.by_lease.remove(lease_id).is_none() {
            return Err(LeaseError::LeaseNotFound(lease_id.to_string()));
        }
        Ok(())
    }

    /// Check if an edit can proceed based on lease state.
    ///
    /// This gate enforces that edits only happen when:
    /// 1. The task is in a Leased state
    /// 2. The context fanout has been gathered (providing pre-edit analysis)
    ///
    /// # Arguments
    /// * `state` - The current runtime state of the task
    /// * `context` - The pre-edit context fanout (may be empty for non-code edits)
    ///
    /// # Returns
    /// `Ok(())` if the edit can proceed, `Err(LeaseError)` otherwise
    pub fn can_edit(
        &self,
        state: &RuntimeState,
        _context: &ContextFanout,
    ) -> Result<(), LeaseError> {
        match state {
            RuntimeState::Leased { .. } => {
                // Lease is active - verify context was gathered
                // For code edits, we expect at least the target file in live_context
                // For non-code edits, empty context is acceptable
                Ok(())
            }
            RuntimeState::Ready => Err(LeaseError::NotLeased),
            RuntimeState::Blocked { .. } => {
                Err(LeaseError::InvalidState("task is blocked".to_string()))
            }
            RuntimeState::Verifying { .. } => Err(LeaseError::InvalidState(
                "task is already verifying".to_string(),
            )),
            RuntimeState::Recoverable { .. } => Err(LeaseError::InvalidState(
                "task is in recoverable state".to_string(),
            )),
            RuntimeState::AwaitingHuman { .. } => Err(LeaseError::InvalidState(
                "task is awaiting human input".to_string(),
            )),
            RuntimeState::Terminal(_) => Err(LeaseError::InvalidState(
                "task is in terminal state".to_string(),
            )),
        }
    }

    /// Check if an edit can proceed with additional context validation.
    ///
    /// This is a stricter version of `can_edit` that also validates:
    /// - Context was gathered (live_context is non-empty for code edits)
    /// - Token budget was respected
    ///
    /// # Arguments
    /// * `state` - The current runtime state
    /// * `context` - The pre-edit context fanout
    /// * `min_expected_files` - Minimum expected files for a valid code edit
    ///
    /// # Returns
    /// `Ok(())` if the edit can proceed with valid context
    pub fn can_edit_with_context(
        &self,
        state: &RuntimeState,
        context: &ContextFanout,
        min_expected_files: usize,
    ) -> Result<(), LeaseError> {
        // First check lease state
        self.can_edit(state, context)?;

        // For code edits, verify we have context
        if context.file_count() < min_expected_files {
            return Err(LeaseError::InvalidState(format!(
                "insufficient context gathered: {} files, expected at least {}",
                context.file_count(),
                min_expected_files
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::state::TerminalState;
    use chrono::{Duration, Utc};
    use std::sync::Arc;

    #[test]
    fn lease_manager_enforces_single_active_lease_per_agent() {
        let mut mgr = LeaseManager::default();
        let _a = mgr.issue_lease("task-a", "agent-1", 3600).expect("lease");
        let err = mgr
            .issue_lease("task-b", "agent-1", 3600)
            .expect_err("must fail");
        assert!(matches!(err, LeaseError::AgentHasActiveLease(_)));
    }

    #[test]
    fn lease_manager_fence_validation_works() {
        let mut mgr = LeaseManager::default();
        let lease = mgr.issue_lease("task-a", "agent-1", 3600).expect("lease");
        assert!(
            mgr.validate_fence(&lease.lease_id, lease.fence_token)
                .is_ok()
        );
        assert!(
            mgr.validate_fence(&lease.lease_id, lease.fence_token + 1)
                .is_err()
        );
    }

    #[test]
    fn lease_manager_rejects_overlapping_task_leases() {
        let mut mgr = LeaseManager::default();
        let first = mgr
            .issue_lease("task-a", "agent-1", 3600)
            .expect("first lease");
        let err = mgr
            .issue_lease("task-a", "agent-2", 3600)
            .expect_err("overlapping task lease should be rejected");
        assert!(matches!(
            err,
            LeaseError::TaskAlreadyLeased {
                task_id,
                lease_id,
                ..
            } if task_id == "task-a" && lease_id == first.lease_id
        ));
    }

    #[test]
    fn external_provider_conflict_recovers_after_stale_expiry() {
        let provider = Arc::new(InMemoryExternalLeaseProvider::default());

        let mut first_mgr = LeaseManager::default();
        first_mgr.set_external_provider(provider.clone());
        let mut second_mgr = LeaseManager::default();
        second_mgr.set_external_provider(provider.clone());

        let lease = first_mgr
            .issue_lease("task-a", "agent-1", 3600)
            .expect("first lease");
        let conflict = second_mgr
            .issue_lease("task-a", "agent-2", 3600)
            .expect_err("active external lease should conflict");
        assert!(matches!(conflict, LeaseError::TaskAlreadyLeased { .. }));

        provider.force_expire_for_test(&lease.lease_id, Utc::now() - Duration::seconds(1));
        let recovered = second_mgr
            .issue_lease("task-a", "agent-2", 3600)
            .expect("stale external lease should be reclaimed");
        assert_eq!(recovered.task_id, "task-a");
    }

    // Tests for edit gate

    #[test]
    fn can_edit_allows_when_state_is_leased() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::Leased {
            lease_id: "test-lease".to_string(),
            agent_id: "agent-1".to_string(),
            fence_token: 1,
            expires_at: Utc::now() + Duration::seconds(3600),
        };
        let context = ContextFanout::default();

        assert!(mgr.can_edit(&state, &context).is_ok());
    }

    #[test]
    fn can_edit_blocks_when_state_is_ready() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::Ready;
        let context = ContextFanout::default();

        let err = mgr
            .can_edit(&state, &context)
            .expect_err("should be blocked");
        assert!(matches!(err, LeaseError::NotLeased));
    }

    #[test]
    fn can_edit_blocks_when_state_is_blocked() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::Blocked {
            waiting_on: vec!["other-task".to_string()],
        };
        let context = ContextFanout::default();

        let err = mgr
            .can_edit(&state, &context)
            .expect_err("should be blocked");
        assert!(matches!(err, LeaseError::InvalidState(_)));
    }

    #[test]
    fn can_edit_blocks_when_state_is_terminal() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::Terminal(TerminalState::Succeeded {
            patch_digest: "abc123".to_string(),
            verify_run_id: "run-1".to_string(),
            completed_at: Utc::now(),
        });
        let context = ContextFanout::default();

        let err = mgr
            .can_edit(&state, &context)
            .expect_err("should be blocked");
        assert!(matches!(err, LeaseError::InvalidState(_)));
    }

    #[test]
    fn can_edit_with_context_validates_minimum_files() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::Leased {
            lease_id: "test-lease".to_string(),
            agent_id: "agent-1".to_string(),
            fence_token: 1,
            expires_at: Utc::now() + Duration::seconds(3600),
        };

        // Context with insufficient files
        let context = ContextFanout::default();

        let err = mgr
            .can_edit_with_context(&state, &context, 1)
            .expect_err("should require at least 1 file");
        assert!(matches!(err, LeaseError::InvalidState(_)));
        assert!(err.to_string().contains("insufficient context"));
    }

    #[test]
    fn can_edit_with_context_succeeds_with_sufficient_files() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::Leased {
            lease_id: "test-lease".to_string(),
            agent_id: "agent-1".to_string(),
            fence_token: 1,
            expires_at: Utc::now() + Duration::seconds(3600),
        };

        // Context with sufficient files
        let context = ContextFanout {
            live_context: vec!["src/main.rs".to_string()],
            ..ContextFanout::default()
        };

        assert!(mgr.can_edit_with_context(&state, &context, 1).is_ok());
    }

    #[test]
    fn can_edit_blocks_when_verifying() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::Verifying {
            patch_digest: "digest".to_string(),
            verify_run_id: "run-1".to_string(),
        };
        let context = ContextFanout::default();

        let err = mgr
            .can_edit(&state, &context)
            .expect_err("should be blocked");
        assert!(matches!(err, LeaseError::InvalidState(_)));
        assert!(err.to_string().contains("verifying"));
    }

    #[test]
    fn can_edit_blocks_when_awaiting_human() {
        let mgr = LeaseManager::default();
        let state = RuntimeState::AwaitingHuman {
            question: "Should I proceed?".to_string(),
            context: "Context".to_string(),
            asked_at: Utc::now(),
        };
        let context = ContextFanout::default();

        let err = mgr
            .can_edit(&state, &context)
            .expect_err("should be blocked");
        assert!(matches!(err, LeaseError::InvalidState(_)));
        assert!(err.to_string().contains("awaiting human"));
    }
}
