//! Recovery state definitions and transitions.
//!
//! This module defines the state machine for OAuth unauthorized recovery,
//! including all states, events, and transition logic.

use serde::{Deserialize, Serialize};

/// Maximum recovery attempts before giving up.
pub const MAX_RECOVERY_ATTEMPTS: u8 = 5;

/// States in the OAuth recovery state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[derive(Default)]
pub enum RecoveryState {
    /// Initial state, recovery not yet started.
    #[default]
    Initial,

    /// Attempting to reload credentials from storage.
    Reloading {
        /// Current reload attempt (1-indexed).
        attempt: u8,
        /// Timestamp when this state was entered.
        started_at: i64,
    },

    /// Attempting to refresh the OAuth token.
    RefreshingToken {
        /// Current refresh attempt (1-indexed).
        attempt: u8,
        /// Provider being refreshed.
        provider: String,
        /// Account ID being refreshed (if applicable).
        account_id: Option<String>,
        /// Timestamp when this state was entered.
        started_at: i64,
    },

    /// Waiting for user to complete manual relogin.
    AwaitingRelogin {
        /// Provider requiring relogin.
        provider: String,
        /// Account ID requiring relogin (if applicable).
        account_id: Option<String>,
        /// Reason why relogin is required.
        reason: String,
    },

    /// Recovery completed successfully.
    Done {
        /// Timestamp when recovery completed.
        recovered_at: i64,
        /// Method used for recovery.
        method: RecoveryMethod,
    },

    /// Recovery failed after exhausting all options.
    Failed {
        /// Reason for failure.
        reason: String,
        /// Final error message if available.
        final_error: Option<String>,
        /// Timestamp when failure was determined.
        failed_at: i64,
    },
}

/// Method used to recover credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryMethod {
    /// Credentials were reloaded from storage (external process updated them).
    StorageReload,

    /// Token was successfully refreshed.
    TokenRefresh,

    /// Account was rotated to a different one in the pool.
    AccountRotation,

    /// User completed manual relogin.
    ManualRelogin,
}

/// Result of a token refresh attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshResult {
    /// Refresh succeeded.
    Success,

    /// Refresh failed but can be retried.
    RetryableFailure,

    /// Refresh failed permanently, requires relogin.
    RequiresRelogin,

    /// Refresh failed due to rate limiting.
    RateLimited,
}

/// Events that drive state transitions in the recovery machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RecoveryEvent {
    /// Start recovery process.
    Start,

    /// Storage reload completed.
    ReloadComplete {
        /// Whether reload found valid credentials.
        success: bool,
    },

    /// Token refresh completed.
    RefreshComplete {
        /// Result of the refresh attempt.
        result: RefreshResult,
    },

    /// Relogin is required (from error response).
    ReloginRequired {
        /// Reason why relogin is required.
        reason: String,
    },

    /// User completed relogin.
    ReloginComplete,

    /// Maximum attempts exceeded.
    MaxAttemptsExceeded,

    /// Reset to initial state.
    Reset,
}

/// Error during state transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecoveryTransitionError {
    /// Invalid event for current state.
    #[error("Invalid event {event:?} for state {state:?}")]
    InvalidTransition { state: String, event: String },

    /// Maximum attempts exceeded.
    #[error("Maximum recovery attempts ({max}) exceeded")]
    MaxAttemptsExceeded { max: u8 },
}

impl RecoveryState {
    /// Transition to the next state based on an event.
    ///
    /// This implements the state machine logic for OAuth recovery.
    pub fn transition(
        &self,
        event: RecoveryEvent,
        config: &super::RecoveryConfig,
        now_ms: i64,
    ) -> Self {
        match (self, event) {
            // Initial -> Reloading (start recovery)
            (Self::Initial, RecoveryEvent::Start) => Self::Reloading {
                attempt: 1,
                started_at: now_ms,
            },

            // Reloading -> Done (reload succeeded)
            (Self::Reloading { .. }, RecoveryEvent::ReloadComplete { success: true }) => {
                Self::Done {
                    recovered_at: now_ms,
                    method: RecoveryMethod::StorageReload,
                }
            }

            // Reloading -> Reloading (retry, within limits)
            (
                Self::Reloading {
                    attempt,
                    started_at,
                },
                RecoveryEvent::ReloadComplete { success: false },
            ) if attempt < &config.max_reload_attempts => Self::Reloading {
                attempt: attempt + 1,
                started_at: *started_at,
            },

            // Reloading -> RefreshingToken (reload failed, move to refresh)
            (Self::Reloading { .. }, RecoveryEvent::ReloadComplete { success: false }) => {
                Self::RefreshingToken {
                    attempt: 1,
                    provider: String::new(),
                    account_id: None,
                    started_at: now_ms,
                }
            }

            // RefreshingToken -> Done (refresh succeeded)
            (
                Self::RefreshingToken { .. },
                RecoveryEvent::RefreshComplete {
                    result: RefreshResult::Success,
                },
            ) => Self::Done {
                recovered_at: now_ms,
                method: RecoveryMethod::TokenRefresh,
            },

            // RefreshingToken -> RefreshingToken (retryable failure, within limits)
            (
                Self::RefreshingToken {
                    attempt,
                    provider,
                    account_id,
                    started_at,
                },
                RecoveryEvent::RefreshComplete {
                    result: RefreshResult::RetryableFailure | RefreshResult::RateLimited,
                },
            ) if attempt < &config.max_refresh_attempts => Self::RefreshingToken {
                attempt: attempt + 1,
                provider: provider.clone(),
                account_id: account_id.clone(),
                started_at: *started_at,
            },

            // RefreshingToken -> AwaitingRelogin (requires relogin)
            (
                Self::RefreshingToken {
                    provider,
                    account_id,
                    ..
                },
                RecoveryEvent::RefreshComplete {
                    result: RefreshResult::RequiresRelogin,
                },
            ) => Self::AwaitingRelogin {
                provider: provider.clone(),
                account_id: account_id.clone(),
                reason: "Token refresh requires relogin".to_string(),
            },

            // RefreshingToken -> AwaitingRelogin (explicit relogin event)
            (
                Self::RefreshingToken {
                    provider,
                    account_id,
                    ..
                },
                RecoveryEvent::ReloginRequired { reason },
            ) => Self::AwaitingRelogin {
                provider: provider.clone(),
                account_id: account_id.clone(),
                reason,
            },

            // RefreshingToken -> Failed (max attempts exceeded)
            (
                Self::RefreshingToken { attempt, .. },
                RecoveryEvent::RefreshComplete {
                    result: RefreshResult::RetryableFailure | RefreshResult::RateLimited,
                },
            ) if attempt >= &config.max_refresh_attempts => Self::Failed {
                reason: "Maximum refresh attempts exceeded".to_string(),
                final_error: None,
                failed_at: now_ms,
            },

            // RefreshingToken -> Failed (max attempts event)
            (Self::RefreshingToken { .. }, RecoveryEvent::MaxAttemptsExceeded) => Self::Failed {
                reason: "Maximum recovery attempts exceeded".to_string(),
                final_error: None,
                failed_at: now_ms,
            },

            // AwaitingRelogin -> Done (relogin completed)
            (Self::AwaitingRelogin { .. }, RecoveryEvent::ReloginComplete) => Self::Done {
                recovered_at: now_ms,
                method: RecoveryMethod::ManualRelogin,
            },

            // Any state -> Initial (reset)
            (_, RecoveryEvent::Reset) => Self::Initial,

            // Invalid transitions keep the current state
            (state, event) => {
                tracing::warn!(
                    current_state = ?state,
                    event = ?event,
                    "Invalid recovery state transition, staying in current state"
                );
                state.clone()
            }
        }
    }

    /// Check if this state represents an active recovery in progress.
    pub const fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Reloading { .. } | Self::RefreshingToken { .. } | Self::AwaitingRelogin { .. }
        )
    }

    /// Check if this is a terminal state.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Failed { .. })
    }

    /// Get a human-readable description of the state.
    pub fn description(&self) -> String {
        match self {
            Self::Initial => "Not started".to_string(),
            Self::Reloading { attempt, .. } => {
                format!("Reloading credentials (attempt {attempt})")
            }
            Self::RefreshingToken {
                attempt, provider, ..
            } => {
                format!("Refreshing token for {provider} (attempt {attempt})")
            }
            Self::AwaitingRelogin {
                provider, reason, ..
            } => {
                format!("Awaiting relogin for {provider}: {reason}")
            }
            Self::Done { method, .. } => {
                format!("Recovery complete via {method:?}")
            }
            Self::Failed { reason, .. } => {
                format!("Recovery failed: {reason}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> super::super::RecoveryConfig {
        super::super::RecoveryConfig::default()
    }

    #[test]
    fn test_initial_state() {
        let state = RecoveryState::Initial;
        assert!(!state.is_active());
        assert!(!state.is_terminal());
        assert_eq!(state.description(), "Not started");
    }

    #[test]
    fn test_transition_start() {
        let state = RecoveryState::Initial;
        let config = default_config();
        let next = state.transition(RecoveryEvent::Start, &config, 1000);
        assert!(matches!(next, RecoveryState::Reloading { attempt: 1, .. }));
        assert!(next.is_active());
    }

    #[test]
    fn test_reload_success() {
        let state = RecoveryState::Reloading {
            attempt: 1,
            started_at: 1000,
        };
        let config = default_config();
        let next = state.transition(
            RecoveryEvent::ReloadComplete { success: true },
            &config,
            1100,
        );
        assert!(matches!(
            next,
            RecoveryState::Done {
                recovered_at: 1100,
                method: RecoveryMethod::StorageReload,
            }
        ));
        assert!(next.is_terminal());
    }

    #[test]
    fn test_reload_fail_then_refresh_success() {
        let config = default_config();

        // Start from reloading
        let state = RecoveryState::Reloading {
            attempt: 1,
            started_at: 1000,
        };

        // Reload fails -> move to refreshing
        let next = state.transition(
            RecoveryEvent::ReloadComplete { success: false },
            &config,
            1100,
        );
        assert!(matches!(next, RecoveryState::RefreshingToken { .. }));

        // Refresh succeeds -> done
        let next = next.transition(
            RecoveryEvent::RefreshComplete {
                result: RefreshResult::Success,
            },
            &config,
            1200,
        );
        assert!(matches!(
            next,
            RecoveryState::Done {
                method: RecoveryMethod::TokenRefresh,
                ..
            }
        ));
    }

    #[test]
    fn test_refresh_requires_relogin() {
        let config = default_config();

        let state = RecoveryState::RefreshingToken {
            attempt: 1,
            provider: "anthropic".to_string(),
            account_id: Some("user@example.com".to_string()),
            started_at: 1000,
        };

        let next = state.transition(
            RecoveryEvent::RefreshComplete {
                result: RefreshResult::RequiresRelogin,
            },
            &config,
            1100,
        );

        assert!(matches!(
            next,
            RecoveryState::AwaitingRelogin {
                ref provider,
                account_id: Some(_),
                reason: _,
            } if provider == "anthropic"
        ));
        assert!(next.is_active());
    }

    #[test]
    fn test_relogin_complete() {
        let config = default_config();

        let state = RecoveryState::AwaitingRelogin {
            provider: "anthropic".to_string(),
            account_id: None,
            reason: "Token expired".to_string(),
        };

        let next = state.transition(RecoveryEvent::ReloginComplete, &config, 1200);

        assert!(matches!(
            next,
            RecoveryState::Done {
                recovered_at: 1200,
                method: RecoveryMethod::ManualRelogin,
            }
        ));
    }

    #[test]
    fn test_max_refresh_attempts_exceeded() {
        let config = default_config();

        let state = RecoveryState::RefreshingToken {
            attempt: 2, // max is 2 by default
            provider: "anthropic".to_string(),
            account_id: None,
            started_at: 1000,
        };

        let next = state.transition(
            RecoveryEvent::RefreshComplete {
                result: RefreshResult::RetryableFailure,
            },
            &config,
            1100,
        );

        assert!(matches!(next, RecoveryState::Failed { .. }));
        assert!(next.is_terminal());
    }

    #[test]
    fn test_reset_from_any_state() {
        let config = default_config();

        let states = vec![
            RecoveryState::Reloading {
                attempt: 1,
                started_at: 1000,
            },
            RecoveryState::RefreshingToken {
                attempt: 1,
                provider: "test".to_string(),
                account_id: None,
                started_at: 1000,
            },
            RecoveryState::AwaitingRelogin {
                provider: "test".to_string(),
                account_id: None,
                reason: "test".to_string(),
            },
            RecoveryState::Done {
                recovered_at: 1000,
                method: RecoveryMethod::TokenRefresh,
            },
            RecoveryState::Failed {
                reason: "test".to_string(),
                final_error: None,
                failed_at: 1000,
            },
        ];

        for state in states {
            let next = state.transition(RecoveryEvent::Reset, &config, 2000);
            assert!(matches!(next, RecoveryState::Initial));
        }
    }

    #[test]
    fn test_invalid_transition_preserves_state() {
        let config = default_config();

        // Can't reload complete from initial state
        let state = RecoveryState::Initial;
        let next = state.transition(
            RecoveryEvent::ReloadComplete { success: true },
            &config,
            1000,
        );
        assert!(matches!(next, RecoveryState::Initial));

        // Can't start from reloading
        let state = RecoveryState::Reloading {
            attempt: 1,
            started_at: 1000,
        };
        let next = state.transition(RecoveryEvent::Start, &config, 1000);
        assert!(matches!(next, RecoveryState::Reloading { .. }));
    }

    #[test]
    fn test_state_descriptions() {
        assert_eq!(
            RecoveryState::Reloading {
                attempt: 2,
                started_at: 1000
            }
            .description(),
            "Reloading credentials (attempt 2)"
        );

        assert_eq!(
            RecoveryState::RefreshingToken {
                attempt: 1,
                provider: "anthropic".to_string(),
                account_id: None,
                started_at: 1000
            }
            .description(),
            "Refreshing token for anthropic (attempt 1)"
        );

        assert_eq!(
            RecoveryState::AwaitingRelogin {
                provider: "anthropic".to_string(),
                account_id: None,
                reason: "Token expired".to_string()
            }
            .description(),
            "Awaiting relogin for anthropic: Token expired"
        );

        assert_eq!(
            RecoveryState::Failed {
                reason: "Max attempts".to_string(),
                final_error: None,
                failed_at: 1000
            }
            .description(),
            "Recovery failed: Max attempts"
        );
    }
}
