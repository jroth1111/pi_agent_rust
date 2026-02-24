//! Unauthorized recovery state machine for OAuth tokens.
//!
//! This module provides a state machine for recovering from 401/403 errors
//! during OAuth token operations. It implements a multi-stage recovery process:
//! 1. Storage reload (fast path)
//! 2. Token refresh (medium path)
//! 3. Manual relogin (slow path)

mod states;

pub use states::{
    MAX_RECOVERY_ATTEMPTS, RecoveryEvent, RecoveryMethod, RecoveryState, RecoveryTransitionError,
    RefreshResult,
};

use serde::{Deserialize, Serialize};

/// Context for recovery operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryContext {
    /// Provider being recovered.
    pub provider: String,
    /// Account ID being recovered (if applicable).
    pub account_id: Option<String>,
    /// Original error that triggered recovery.
    pub trigger_error: Option<String>,
    /// HTTP status code that triggered recovery.
    pub trigger_status_code: Option<u16>,
    /// Timestamp when recovery started.
    pub started_at_ms: i64,
}

impl RecoveryContext {
    /// Create a new recovery context.
    pub const fn new(provider: String, account_id: Option<String>, now_ms: i64) -> Self {
        Self {
            provider,
            account_id,
            trigger_error: None,
            trigger_status_code: None,
            started_at_ms: now_ms,
        }
    }

    /// Set the trigger error.
    pub fn with_trigger_error(mut self, error: String) -> Self {
        self.trigger_error = Some(error);
        self
    }

    /// Set the trigger status code.
    pub const fn with_status_code(mut self, code: u16) -> Self {
        self.trigger_status_code = Some(code);
        self
    }
}

/// Result of a recovery attempt.
#[derive(Debug, Clone)]
pub enum RecoveryResult {
    /// Recovery succeeded with the given method.
    Success {
        method: RecoveryMethod,
        duration_ms: u64,
    },
    /// Recovery requires user action (relogin).
    RequiresUserAction {
        provider: String,
        account_id: Option<String>,
        reason: String,
    },
    /// Recovery failed after exhausting all options.
    Failed {
        reason: String,
        attempts: u8,
        duration_ms: u64,
    },
    /// Recovery is still in progress.
    InProgress { state: RecoveryState },
}

/// Configuration for recovery behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    /// Maximum number of reload attempts before moving to refresh.
    pub max_reload_attempts: u8,
    /// Maximum number of refresh attempts before requiring relogin.
    pub max_refresh_attempts: u8,
    /// Delay between reload attempts (milliseconds).
    pub reload_delay_ms: u64,
    /// Delay between refresh attempts (milliseconds).
    pub refresh_delay_ms: u64,
    /// Whether recovery is enabled.
    pub enabled: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            max_reload_attempts: 1,
            max_refresh_attempts: 2,
            reload_delay_ms: 100,
            refresh_delay_ms: 1000,
            enabled: true,
        }
    }
}

/// State machine for OAuth unauthorized recovery.
#[derive(Debug, Clone)]
pub struct RecoveryMachine {
    state: RecoveryState,
    context: RecoveryContext,
    config: RecoveryConfig,
}

impl RecoveryMachine {
    /// Create a new recovery machine in the initial state.
    pub const fn new(context: RecoveryContext, config: RecoveryConfig) -> Self {
        Self {
            state: RecoveryState::Initial,
            context,
            config,
        }
    }

    /// Get the current state.
    pub const fn state(&self) -> &RecoveryState {
        &self.state
    }

    /// Get the recovery context.
    pub const fn context(&self) -> &RecoveryContext {
        &self.context
    }

    /// Check if recovery is in a terminal state.
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            RecoveryState::Done { .. } | RecoveryState::Failed { .. }
        )
    }

    /// Process an event and transition to the next state.
    pub fn process_event(&mut self, event: RecoveryEvent, now_ms: i64) -> RecoveryResult {
        let next_state =
            self.normalize_state_context(self.state.transition(event, &self.config, now_ms));

        // Extract result information based on the transition
        let result = match &next_state {
            RecoveryState::Done {
                method,
                recovered_at,
            } => {
                let duration = recovered_at.saturating_sub(self.context.started_at_ms);
                RecoveryResult::Success {
                    method: method.clone(),
                    duration_ms: duration.max(0) as u64,
                }
            }
            RecoveryState::Failed {
                reason, failed_at, ..
            } => {
                let duration = failed_at.saturating_sub(self.context.started_at_ms);
                RecoveryResult::Failed {
                    reason: reason.clone(),
                    attempts: self.attempt_count(),
                    duration_ms: duration.max(0) as u64,
                }
            }
            RecoveryState::AwaitingRelogin {
                provider,
                account_id,
                reason,
            } => RecoveryResult::RequiresUserAction {
                provider: provider.clone(),
                account_id: account_id.clone(),
                reason: reason.clone(),
            },
            _ => RecoveryResult::InProgress {
                state: next_state.clone(),
            },
        };

        self.state = next_state;
        result
    }

    fn normalize_state_context(&self, state: RecoveryState) -> RecoveryState {
        match state {
            RecoveryState::RefreshingToken {
                attempt,
                provider,
                account_id,
                started_at,
            } => RecoveryState::RefreshingToken {
                attempt,
                provider: if provider.is_empty() {
                    self.context.provider.clone()
                } else {
                    provider
                },
                account_id: account_id.or_else(|| self.context.account_id.clone()),
                started_at,
            },
            RecoveryState::AwaitingRelogin {
                provider,
                account_id,
                reason,
            } => RecoveryState::AwaitingRelogin {
                provider: if provider.is_empty() {
                    self.context.provider.clone()
                } else {
                    provider
                },
                account_id: account_id.or_else(|| self.context.account_id.clone()),
                reason,
            },
            _ => state,
        }
    }

    /// Get the current attempt count across all stages.
    const fn attempt_count(&self) -> u8 {
        match &self.state {
            RecoveryState::Initial => 0,
            RecoveryState::Reloading { attempt, .. } => *attempt,
            RecoveryState::RefreshingToken { attempt, .. } => *attempt,
            RecoveryState::AwaitingRelogin { .. } => {
                self.config.max_reload_attempts + self.config.max_refresh_attempts
            }
            RecoveryState::Done { .. } | RecoveryState::Failed { .. } => {
                self.config.max_reload_attempts + self.config.max_refresh_attempts
            }
        }
    }

    /// Start recovery by transitioning from Initial to Reloading.
    pub fn start(&mut self, now_ms: i64) -> RecoveryResult {
        if !self.config.enabled {
            self.state = RecoveryState::Failed {
                reason: "Recovery disabled by configuration".to_string(),
                final_error: None,
                failed_at: now_ms,
            };
            return RecoveryResult::Failed {
                reason: "Recovery disabled by configuration".to_string(),
                attempts: 0,
                duration_ms: 0,
            };
        }

        self.process_event(RecoveryEvent::Start, now_ms)
    }

    /// Reset the machine to initial state for a new recovery attempt.
    pub fn reset(&mut self) {
        self.state = RecoveryState::Initial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recovery_context_creation() {
        let ctx = RecoveryContext::new(
            "anthropic".to_string(),
            Some("user@example.com".to_string()),
            1000,
        );
        assert_eq!(ctx.provider, "anthropic");
        assert_eq!(ctx.account_id, Some("user@example.com".to_string()));
        assert_eq!(ctx.started_at_ms, 1000);
        assert!(ctx.trigger_error.is_none());
        assert!(ctx.trigger_status_code.is_none());
    }

    #[test]
    fn test_recovery_context_with_options() {
        let ctx = RecoveryContext::new("anthropic".to_string(), None, 1000)
            .with_trigger_error("invalid_token".to_string())
            .with_status_code(401);
        assert_eq!(ctx.trigger_error, Some("invalid_token".to_string()));
        assert_eq!(ctx.trigger_status_code, Some(401));
    }

    #[test]
    fn test_recovery_config_default() {
        let config = RecoveryConfig::default();
        assert_eq!(config.max_reload_attempts, 1);
        assert_eq!(config.max_refresh_attempts, 2);
        assert!(config.enabled);
    }

    #[test]
    fn test_recovery_machine_start() {
        let ctx = RecoveryContext::new("anthropic".to_string(), None, 1000);
        let config = RecoveryConfig::default();
        let mut machine = RecoveryMachine::new(ctx, config);

        let result = machine.start(1000);
        assert!(matches!(result, RecoveryResult::InProgress { .. }));
        assert!(matches!(machine.state(), RecoveryState::Reloading { .. }));
    }

    #[test]
    fn test_recovery_machine_disabled() {
        let ctx = RecoveryContext::new("anthropic".to_string(), None, 1000);
        let config = RecoveryConfig {
            enabled: false,
            ..Default::default()
        };
        let mut machine = RecoveryMachine::new(ctx, config);

        let result = machine.start(1000);
        assert!(matches!(result, RecoveryResult::Failed { .. }));
        assert!(machine.is_terminal());
    }

    #[test]
    fn test_recovery_success_path() {
        let ctx = RecoveryContext::new("anthropic".to_string(), None, 1000);
        let config = RecoveryConfig::default();
        let mut machine = RecoveryMachine::new(ctx, config);

        // Start recovery
        machine.start(1000);
        assert!(matches!(machine.state(), RecoveryState::Reloading { .. }));

        // Reload succeeds
        let result = machine.process_event(RecoveryEvent::ReloadComplete { success: true }, 1100);
        assert!(matches!(result, RecoveryResult::Success { .. }));
        assert!(machine.is_terminal());
    }

    #[test]
    fn test_recovery_reload_fail_refresh_success() {
        let ctx = RecoveryContext::new(
            "anthropic".to_string(),
            Some("user@example.com".to_string()),
            1000,
        );
        let config = RecoveryConfig::default();
        let mut machine = RecoveryMachine::new(ctx, config);

        // Start recovery
        machine.start(1000);

        // Reload fails
        let result = machine.process_event(RecoveryEvent::ReloadComplete { success: false }, 1100);
        assert!(matches!(result, RecoveryResult::InProgress { .. }));

        // Refresh succeeds
        let result = machine.process_event(
            RecoveryEvent::RefreshComplete {
                result: states::RefreshResult::Success,
            },
            1200,
        );
        assert!(matches!(
            result,
            RecoveryResult::Success {
                method: RecoveryMethod::TokenRefresh,
                ..
            }
        ));
    }

    #[test]
    fn test_recovery_requires_relogin() {
        let ctx = RecoveryContext::new(
            "anthropic".to_string(),
            Some("user@example.com".to_string()),
            1000,
        );
        let config = RecoveryConfig::default();
        let mut machine = RecoveryMachine::new(ctx, config);

        // Start recovery
        machine.start(1000);

        // Reload fails
        machine.process_event(RecoveryEvent::ReloadComplete { success: false }, 1100);

        // Refresh requires relogin
        let result = machine.process_event(
            RecoveryEvent::RefreshComplete {
                result: states::RefreshResult::RequiresRelogin,
            },
            1200,
        );
        assert!(matches!(result, RecoveryResult::RequiresUserAction { .. }));
        assert!(matches!(
            machine.state(),
            RecoveryState::AwaitingRelogin { .. }
        ));
        assert!(!machine.is_terminal());
    }

    #[test]
    fn test_recovery_relogin_complete() {
        let ctx = RecoveryContext::new(
            "anthropic".to_string(),
            Some("user@example.com".to_string()),
            1000,
        );
        let config = RecoveryConfig::default();
        let mut machine = RecoveryMachine::new(ctx, config);

        // Get to awaiting relogin state
        machine.start(1000);
        machine.process_event(RecoveryEvent::ReloadComplete { success: false }, 1100);
        machine.process_event(
            RecoveryEvent::RefreshComplete {
                result: states::RefreshResult::RequiresRelogin,
            },
            1200,
        );

        // Relogin completes
        let result = machine.process_event(RecoveryEvent::ReloginComplete, 1300);
        assert!(matches!(
            result,
            RecoveryResult::Success {
                method: RecoveryMethod::ManualRelogin,
                ..
            }
        ));
        assert!(machine.is_terminal());
    }

    #[test]
    fn test_recovery_machine_reset() {
        let ctx = RecoveryContext::new("anthropic".to_string(), None, 1000);
        let config = RecoveryConfig::default();
        let mut machine = RecoveryMachine::new(ctx, config);

        machine.start(1000);
        machine.process_event(RecoveryEvent::ReloadComplete { success: true }, 1100);
        assert!(machine.is_terminal());

        machine.reset();
        assert!(matches!(machine.state(), RecoveryState::Initial));
        assert!(!machine.is_terminal());
    }

    #[test]
    fn test_recovery_context_preserved_on_refresh_and_relogin_states() {
        let ctx = RecoveryContext::new(
            "openai-codex".to_string(),
            Some("acct-42".to_string()),
            1000,
        );
        let config = RecoveryConfig::default();
        let mut machine = RecoveryMachine::new(ctx, config);

        machine.start(1000);
        machine.process_event(RecoveryEvent::ReloadComplete { success: false }, 1100);

        match machine.state() {
            RecoveryState::RefreshingToken {
                provider,
                account_id,
                ..
            } => {
                assert_eq!(provider, "openai-codex");
                assert_eq!(account_id.as_deref(), Some("acct-42"));
            }
            state => panic!("expected RefreshingToken, got {state:?}"),
        }

        machine.process_event(
            RecoveryEvent::RefreshComplete {
                result: states::RefreshResult::RequiresRelogin,
            },
            1200,
        );

        match machine.state() {
            RecoveryState::AwaitingRelogin {
                provider,
                account_id,
                ..
            } => {
                assert_eq!(provider, "openai-codex");
                assert_eq!(account_id.as_deref(), Some("acct-42"));
            }
            state => panic!("expected AwaitingRelogin, got {state:?}"),
        }
    }
}
