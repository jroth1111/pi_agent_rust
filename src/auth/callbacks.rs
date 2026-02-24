//! Event callbacks for OAuth token refresh observability.
//!
//! This module provides a callback infrastructure for monitoring OAuth token
//! refresh operations, allowing consumers to react to refresh success, failure,
//! relogin requirements, and rate limiting events.

// OAuthErrorClass is defined in the parent auth module
use super::OAuthErrorClass;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Events emitted during OAuth token refresh operations.
///
/// These events provide observability into the lifecycle of token refresh
/// attempts, enabling monitoring, alerting, and user notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RefreshEvent {
    /// Token refresh completed successfully.
    Success {
        /// Provider identifier (e.g., "anthropic", "openai-codex").
        provider: String,
        /// Account identifier (e.g., email or account ID).
        account_id: String,
        /// Unix timestamp (milliseconds) when the new token expires.
        new_expires_at: i64,
        /// Duration of the refresh operation in milliseconds.
        duration_ms: u64,
    },
    /// Token refresh failed.
    Failure {
        /// Provider identifier.
        provider: String,
        /// Account identifier.
        account_id: String,
        /// Human-readable error message.
        error: String,
        /// Error classification for retry behavior.
        error_class: OAuthErrorClass,
        /// Whether the refresh will be retried automatically.
        will_retry: bool,
    },
    /// Token refresh requires the user to reauthenticate.
    RequiresRelogin {
        /// Provider identifier.
        provider: String,
        /// Account identifier.
        account_id: String,
        /// Reason why relogin is required.
        reason: String,
    },
    /// Token refresh was rate limited by the provider.
    RateLimited {
        /// Provider identifier.
        provider: String,
        /// Account identifier.
        account_id: String,
        /// Milliseconds until the next retry is allowed, if specified.
        retry_after_ms: Option<i64>,
    },
}

impl RefreshEvent {
    /// Returns the provider identifier for this event.
    pub fn provider(&self) -> &str {
        match self {
            Self::Success { provider, .. }
            | Self::Failure { provider, .. }
            | Self::RequiresRelogin { provider, .. }
            | Self::RateLimited { provider, .. } => provider,
        }
    }

    /// Returns the account identifier for this event.
    pub fn account_id(&self) -> &str {
        match self {
            Self::Success { account_id, .. }
            | Self::Failure { account_id, .. }
            | Self::RequiresRelogin { account_id, .. }
            | Self::RateLimited { account_id, .. } => account_id,
        }
    }
}

/// Trait for handling OAuth token refresh events.
///
/// Implementations can log events, send notifications, update metrics,
/// or trigger other side effects in response to refresh operations.
pub trait RefreshCallback: Sync + Send {
    /// Handle a refresh event.
    ///
    /// Implementations should avoid blocking or performing expensive operations
    /// synchronously, as this is called during the refresh path.
    fn on_refresh_event(&self, event: &RefreshEvent);
}

/// Callback that logs refresh events using `tracing`.
///
/// Each event type is logged at an appropriate level:
/// - `Success`: INFO
/// - `Failure`: ERROR (if fatal) or WARN (if retryable)
/// - `RequiresRelogin`: WARN
/// - `RateLimited`: WARN
#[derive(Debug, Clone, Default)]
pub struct LoggingRefreshCallback;

impl RefreshCallback for LoggingRefreshCallback {
    fn on_refresh_event(&self, event: &RefreshEvent) {
        match event {
            RefreshEvent::Success {
                provider,
                account_id,
                new_expires_at,
                duration_ms,
            } => {
                tracing::info!(
                    provider = %provider,
                    account_id = %account_id,
                    expires_at = new_expires_at,
                    duration_ms = *duration_ms,
                    "OAuth token refreshed successfully"
                );
            }
            RefreshEvent::Failure {
                provider,
                account_id,
                error,
                error_class,
                will_retry,
            } => {
                let level = match error_class {
                    OAuthErrorClass::Auth | OAuthErrorClass::Fatal => tracing::Level::ERROR,
                    _ => tracing::Level::WARN,
                };
                if level == tracing::Level::ERROR {
                    tracing::error!(
                        provider = %provider,
                        account_id = %account_id,
                        error = %error,
                        error_class = ?error_class,
                        will_retry = *will_retry,
                        "OAuth token refresh failed"
                    );
                } else {
                    tracing::warn!(
                        provider = %provider,
                        account_id = %account_id,
                        error = %error,
                        error_class = ?error_class,
                        will_retry = *will_retry,
                        "OAuth token refresh failed"
                    );
                }
            }
            RefreshEvent::RequiresRelogin {
                provider,
                account_id,
                reason,
            } => {
                tracing::warn!(
                    provider = %provider,
                    account_id = %account_id,
                    reason = %reason,
                    "OAuth token refresh requires relogin"
                );
            }
            RefreshEvent::RateLimited {
                provider,
                account_id,
                retry_after_ms,
            } => {
                tracing::warn!(
                    provider = %provider,
                    account_id = %account_id,
                    retry_after_ms = ?retry_after_ms,
                    "OAuth token refresh rate limited"
                );
            }
        }
    }
}

/// Callback for TUI notifications during refresh events.
///
/// This is a placeholder implementation that can be connected to the
/// actual TUI notification system when available.
#[derive(Debug, Clone, Default)]
pub struct NotificationRefreshCallback;

impl RefreshCallback for NotificationRefreshCallback {
    fn on_refresh_event(&self, event: &RefreshEvent) {
        match event {
            RefreshEvent::Failure {
                provider,
                account_id,
                error,
                will_retry: false,
                ..
            } => {
                tracing::debug!(
                    provider = %provider,
                    account_id = %account_id,
                    error = %error,
                    "Notification: permanent OAuth refresh failure"
                );
            }
            RefreshEvent::RequiresRelogin {
                provider,
                account_id,
                reason,
            } => {
                tracing::debug!(
                    provider = %provider,
                    account_id = %account_id,
                    reason = %reason,
                    "Notification: OAuth relogin required"
                );
            }
            _ => {
                // Other events don't require user-facing notifications
            }
        }
    }
}

/// Registry for managing multiple refresh callbacks.
///
/// Maintains a thread-safe collection of callbacks and dispatches events
/// to all registered listeners.
#[derive(Default)]
pub struct CallbackRegistry {
    callbacks: Arc<Mutex<Vec<Arc<dyn RefreshCallback>>>>,
}

impl std::fmt::Debug for CallbackRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.callbacks.lock().map(|c| c.len()).unwrap_or(0);
        f.debug_struct("CallbackRegistry")
            .field("callback_count", &count)
            .finish()
    }
}

impl CallbackRegistry {
    /// Create a new empty callback registry.
    pub fn new() -> Self {
        Self {
            callbacks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a callback to receive refresh events.
    ///
    /// The callback will be invoked for all future refresh events.
    pub fn register_callback(&self, callback: Arc<dyn RefreshCallback>) {
        let mut callbacks = self.callbacks.lock().expect("callback registry lock");
        callbacks.push(callback);
    }

    /// Remove all registered callbacks.
    ///
    /// This is primarily useful for testing or resetting state.
    pub fn clear(&self) {
        let mut callbacks = self.callbacks.lock().expect("callback registry lock");
        callbacks.clear();
    }

    /// Dispatch an event to all registered callbacks.
    ///
    /// Each callback is invoked in sequence; errors in individual callbacks
    /// do not affect delivery to other callbacks.
    pub fn notify_all(&self, event: &RefreshEvent) {
        let callbacks = self.callbacks.lock().expect("callback registry lock");
        for callback in callbacks.iter() {
            callback.on_refresh_event(event);
        }
    }

    /// Return the number of registered callbacks.
    pub fn len(&self) -> usize {
        let callbacks = self.callbacks.lock().expect("callback registry lock");
        callbacks.len()
    }

    /// Return whether any callbacks are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test callback that counts invocations.
    #[derive(Debug, Default, Clone)]
    struct CountingCallback {
        count: Arc<AtomicUsize>,
    }

    impl RefreshCallback for CountingCallback {
        fn on_refresh_event(&self, _event: &RefreshEvent) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Test callback that records received events.
    #[derive(Debug, Default, Clone)]
    struct RecordingCallback {
        events: Arc<Mutex<Vec<RefreshEvent>>>,
    }

    impl RefreshCallback for RecordingCallback {
        fn on_refresh_event(&self, event: &RefreshEvent) {
            let mut events = self.events.lock().unwrap();
            events.push(event.clone());
        }
    }

    #[test]
    fn test_event_creation() {
        let event = RefreshEvent::Success {
            provider: "anthropic".to_string(),
            account_id: "user@example.com".to_string(),
            new_expires_at: 1_234_567_890_000,
            duration_ms: 123,
        };
        assert_eq!(event.provider(), "anthropic");
        assert_eq!(event.account_id(), "user@example.com");
    }

    #[test]
    fn test_event_failure_creation() {
        let event = RefreshEvent::Failure {
            provider: "openai-codex".to_string(),
            account_id: "user@example.com".to_string(),
            error: "invalid_grant".to_string(),
            error_class: OAuthErrorClass::Auth,
            will_retry: false,
        };
        assert_eq!(event.provider(), "openai-codex");
        assert_eq!(event.account_id(), "user@example.com");
    }

    #[test]
    fn test_callback_invocation() {
        let callback = CountingCallback::default();
        let event = RefreshEvent::Success {
            provider: "test".to_string(),
            account_id: "test@example.com".to_string(),
            new_expires_at: 0,
            duration_ms: 0,
        };

        assert_eq!(callback.count.load(Ordering::SeqCst), 0);
        callback.on_refresh_event(&event);
        assert_eq!(callback.count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_logging_callback() {
        let callback = LoggingRefreshCallback::default();
        let event = RefreshEvent::Success {
            provider: "anthropic".to_string(),
            account_id: "user@example.com".to_string(),
            new_expires_at: 1_234_567_890_000,
            duration_ms: 123,
        };
        // Should not panic
        callback.on_refresh_event(&event);
    }

    #[test]
    fn test_notification_callback() {
        let callback = NotificationRefreshCallback::default();
        let event = RefreshEvent::RequiresRelogin {
            provider: "anthropic".to_string(),
            account_id: "user@example.com".to_string(),
            reason: "invalid_grant".to_string(),
        };
        // Should not panic
        callback.on_refresh_event(&event);
    }

    #[test]
    fn test_registry_single_callback() {
        let registry = CallbackRegistry::new();
        let callback = CountingCallback::default();
        registry.register_callback(Arc::new(callback.clone()));

        let event = RefreshEvent::Success {
            provider: "test".to_string(),
            account_id: "test@example.com".to_string(),
            new_expires_at: 0,
            duration_ms: 0,
        };

        assert_eq!(callback.count.load(Ordering::SeqCst), 0);
        registry.notify_all(&event);
        assert_eq!(callback.count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_registry_multiple_callbacks() {
        let registry = CallbackRegistry::new();
        let callback1 = CountingCallback::default();
        let callback2 = CountingCallback::default();
        let callback3 = CountingCallback::default();

        registry.register_callback(Arc::new(callback1.clone()));
        registry.register_callback(Arc::new(callback2.clone()));
        registry.register_callback(Arc::new(callback3.clone()));

        let event = RefreshEvent::Success {
            provider: "test".to_string(),
            account_id: "test@example.com".to_string(),
            new_expires_at: 0,
            duration_ms: 0,
        };

        registry.notify_all(&event);

        assert_eq!(callback1.count.load(Ordering::SeqCst), 1);
        assert_eq!(callback2.count.load(Ordering::SeqCst), 1);
        assert_eq!(callback3.count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_registry_event_recording() {
        let registry = CallbackRegistry::new();
        let recorder = RecordingCallback::default();
        registry.register_callback(Arc::new(recorder.clone()));

        let event1 = RefreshEvent::Success {
            provider: "anthropic".to_string(),
            account_id: "user@example.com".to_string(),
            new_expires_at: 1_234_567_890_000,
            duration_ms: 123,
        };

        let event2 = RefreshEvent::Failure {
            provider: "openai-codex".to_string(),
            account_id: "user@example.com".to_string(),
            error: "invalid_grant".to_string(),
            error_class: OAuthErrorClass::Auth,
            will_retry: false,
        };

        registry.notify_all(&event1);
        registry.notify_all(&event2);

        let events = recorder.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], RefreshEvent::Success { .. }));
        assert!(matches!(events[1], RefreshEvent::Failure { .. }));
    }

    #[test]
    fn test_registry_clear() {
        let registry = CallbackRegistry::new();
        let callback = CountingCallback::default();
        registry.register_callback(Arc::new(callback.clone()));

        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());

        registry.clear();

        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());

        let event = RefreshEvent::Success {
            provider: "test".to_string(),
            account_id: "test@example.com".to_string(),
            new_expires_at: 0,
            duration_ms: 0,
        };

        registry.notify_all(&event);
        // Callback should not be invoked after clear
        assert_eq!(callback.count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_event_all_variants() {
        let recorder = RecordingCallback::default();

        let events = vec![
            RefreshEvent::Success {
                provider: "p1".to_string(),
                account_id: "a1".to_string(),
                new_expires_at: 1000,
                duration_ms: 100,
            },
            RefreshEvent::Failure {
                provider: "p2".to_string(),
                account_id: "a2".to_string(),
                error: "error".to_string(),
                error_class: OAuthErrorClass::Transport,
                will_retry: true,
            },
            RefreshEvent::RequiresRelogin {
                provider: "p3".to_string(),
                account_id: "a3".to_string(),
                reason: "reason".to_string(),
            },
            RefreshEvent::RateLimited {
                provider: "p4".to_string(),
                account_id: "a4".to_string(),
                retry_after_ms: Some(5000),
            },
        ];

        for event in &events {
            recorder.on_refresh_event(event);
        }

        let recorded = recorder.events.lock().unwrap();
        assert_eq!(recorded.len(), 4);
    }

    #[test]
    fn test_logging_callback_all_variants() {
        let callback = LoggingRefreshCallback::default();

        // All event variants should log without panicking
        callback.on_refresh_event(&RefreshEvent::Success {
            provider: "p1".to_string(),
            account_id: "a1".to_string(),
            new_expires_at: 1000,
            duration_ms: 100,
        });

        callback.on_refresh_event(&RefreshEvent::Failure {
            provider: "p2".to_string(),
            account_id: "a2".to_string(),
            error: "auth error".to_string(),
            error_class: OAuthErrorClass::Auth,
            will_retry: false,
        });

        callback.on_refresh_event(&RefreshEvent::Failure {
            provider: "p3".to_string(),
            account_id: "a3".to_string(),
            error: "rate limited".to_string(),
            error_class: OAuthErrorClass::RateLimit,
            will_retry: true,
        });

        callback.on_refresh_event(&RefreshEvent::RequiresRelogin {
            provider: "p4".to_string(),
            account_id: "a4".to_string(),
            reason: "expired".to_string(),
        });

        callback.on_refresh_event(&RefreshEvent::RateLimited {
            provider: "p5".to_string(),
            account_id: "a5".to_string(),
            retry_after_ms: Some(5000),
        });
    }
}
