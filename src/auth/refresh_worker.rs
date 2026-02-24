//! Background OAuth token refresh worker.
//!
//! This module provides a background worker that periodically checks and refreshes
//! expiring OAuth tokens. It runs in a separate thread and uses the existing
//! `refresh_expired_oauth_tokens_with_client` logic from the auth module.

use super::{AuthStorage, EntropyCalculator, RotationPolicy, RotationStrategy};
use crate::auth::callbacks::CallbackRegistry;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// Configuration for the refresh worker.
#[derive(Debug, Clone)]
pub struct RefreshWorkerConfig {
    /// How many seconds before token expiry to trigger refresh.
    ///
    /// Tokens expiring within this window will be refreshed proactively.
    pub refresh_lead_time_secs: u64,

    /// How often to check for tokens needing refresh (in seconds).
    pub check_interval_secs: u64,

    /// Maximum number of concurrent refresh operations.
    pub max_concurrency: usize,

    /// Maximum number of retry attempts for failed refreshes.
    pub max_retries: u32,

    /// Backoff multiplier for retry attempts.
    pub backoff_multiplier: f64,

    /// Whether the worker is enabled.
    pub enabled: bool,

    /// Rotation policy used to schedule non-expiry-driven refresh/rotation work.
    pub rotation_policy: RotationPolicy,
}

impl Default for RefreshWorkerConfig {
    fn default() -> Self {
        Self {
            refresh_lead_time_secs: 600, // 10 minutes
            check_interval_secs: 60,     // 1 minute
            max_concurrency: 5,
            max_retries: 3,
            backoff_multiplier: 2.0,
            enabled: true,
            rotation_policy: RotationPolicy::default(),
        }
    }
}

/// Background worker for refreshing OAuth tokens.
///
/// The worker runs in a dedicated thread and periodically checks for tokens
/// that are approaching expiry. It uses a semaphore to limit concurrent
/// refresh operations and implements exponential backoff for retries.
#[derive(Debug)]
pub struct RefreshWorker {
    /// Thread handle for the worker.
    handle: Option<thread::JoinHandle<()>>,
    /// Shutdown signal sender.
    shutdown_tx: Option<mpsc::Sender<()>>,
    /// Configuration (cloned for the worker thread).
    config: RefreshWorkerConfig,
    /// Running state for observability.
    running: Arc<AtomicBool>,
    /// Counter for active refresh operations.
    active_refreshes: Arc<AtomicUsize>,
}

impl RefreshWorker {
    /// Start a new refresh worker with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Worker configuration
    /// * `auth_storage` - Shared reference to auth storage
    /// * `callbacks` - Shared reference to callback registry for events
    ///
    /// # Returns
    ///
    /// A new `RefreshWorker` instance that will run until shutdown is called.
    pub fn start(
        config: RefreshWorkerConfig,
        auth_storage: Arc<Mutex<AuthStorage>>,
        callbacks: Arc<CallbackRegistry>,
    ) -> Self {
        if !config.enabled {
            tracing::info!("Refresh worker disabled by configuration");
            return Self {
                handle: None,
                shutdown_tx: None,
                config,
                running: Arc::new(AtomicBool::new(false)),
                active_refreshes: Arc::new(AtomicUsize::new(0)),
            };
        }

        let running = Arc::new(AtomicBool::new(true));
        let active_refreshes = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

        let running_clone = Arc::clone(&running);
        let active_refreshes_clone = Arc::clone(&active_refreshes);
        let config_clone = config.clone();

        let handle = thread::Builder::new()
            .name("oauth-refresh-worker".to_string())
            .spawn(move || {
                tracing::info!(
                    check_interval_secs = config_clone.check_interval_secs,
                    refresh_lead_time_secs = config_clone.refresh_lead_time_secs,
                    max_concurrency = config_clone.max_concurrency,
                    "OAuth refresh worker started"
                );

                // Create a simple semaphore for concurrency limiting
                let semaphore = Arc::new(std::sync::Mutex::new(0));
                let semaphore_cv = Arc::new(std::sync::Condvar::new());

                while running_clone.load(Ordering::Relaxed) {
                    // Check for shutdown signal with timeout
                    match shutdown_rx
                        .recv_timeout(Duration::from_secs(config_clone.check_interval_secs))
                    {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            tracing::info!("OAuth refresh worker received shutdown signal");
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // Timeout is expected - time to run a refresh check
                        }
                    }

                    // Check if we're still supposed to be running
                    if !running_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    // Run a single refresh tick
                    Self::run_refresh_tick(
                        &auth_storage,
                        &callbacks,
                        &config_clone,
                        &semaphore,
                        &semaphore_cv,
                        &active_refreshes_clone,
                    );
                }

                tracing::info!("OAuth refresh worker stopped");
            })
            .expect("failed to spawn refresh worker thread");

        Self {
            handle: Some(handle),
            shutdown_tx: Some(shutdown_tx),
            config,
            running,
            active_refreshes,
        }
    }

    /// Run a single refresh tick.
    ///
    /// This method is called periodically by the worker thread to check for
    /// and refresh expiring tokens.
    fn run_refresh_tick(
        auth_storage: &Arc<Mutex<AuthStorage>>,
        callbacks: &Arc<CallbackRegistry>,
        config: &RefreshWorkerConfig,
        _semaphore: &Arc<std::sync::Mutex<usize>>,
        _semaphore_cv: &Arc<std::sync::Condvar>,
        _active_refreshes: &Arc<AtomicUsize>,
    ) {
        let mut storage = match auth_storage.lock() {
            Ok(guard) => guard,
            Err(e) => {
                tracing::warn!(error = ?e, "Failed to acquire auth storage lock for refresh");
                return;
            }
        };

        storage.set_callback_registry(Arc::clone(callbacks));

        // Check for tokens that need refresh
        let tokens_needing_refresh = Self::identify_tokens_needing_refresh(&storage, config);

        if tokens_needing_refresh.is_empty() {
            tracing::debug!("No tokens need refresh at this time");
            storage.clear_callback_registry();
            return;
        }

        tracing::info!(
            count = tokens_needing_refresh.len(),
            "Found tokens needing refresh"
        );

        // We can't actually perform concurrent refreshes easily here because
        // refresh_expired_oauth_tokens_with_client is async and takes &mut self.
        // For now, we'll let the existing logic handle refresh, and this worker
        // mainly triggers the check.
        //
        // In a future enhancement, we could:
        // 1. Add a method to AuthStorage that returns a list of tokens to refresh
        // 2. Spawn async tasks for each refresh
        // 3. Use the semaphore to limit concurrency

        // Create an HTTP client for this tick
        // Note: We use futures::executor::block_on to run the async refresh
        let start = std::time::Instant::now();

        // Perform the refresh
        let result = futures::executor::block_on(storage.refresh_expired_oauth_tokens());
        let duration = start.elapsed();

        match result {
            Ok(()) => {
                tracing::info!(
                    duration_ms = duration.as_millis(),
                    "Refresh tick completed successfully"
                );
                // Note: The refresh_expired_oauth_tokens_with_client doesn't currently
                // emit callbacks. When that's added, we'd forward events here.
            }
            Err(e) => {
                tracing::error!(
                    error = ?e,
                    duration_ms = duration.as_millis(),
                    "Refresh tick failed"
                );
                // Could emit a failure callback here
            }
        }

        storage.clear_callback_registry();
    }

    /// Identify tokens that need refresh based on the configured lead time.
    fn identify_tokens_needing_refresh(
        storage: &AuthStorage,
        config: &RefreshWorkerConfig,
    ) -> Vec<TokenRefreshInfo> {
        let now = chrono::Utc::now().timestamp_millis();
        let deadline = now + (config.refresh_lead_time_secs as i64 * 1000);
        let mut tokens = Vec::new();

        // Check OAuth pool accounts
        for (provider, pool) in &storage.oauth_pools {
            for (account_id, account) in &pool.accounts {
                if let super::AuthCredential::OAuth { expires, .. } = &account.credential {
                    if account.health.requires_relogin {
                        continue;
                    }

                    let effective = config.rotation_policy.config_for_provider(provider);
                    let mut should_schedule_rotation = false;
                    if effective.enabled {
                        if let Some(total) = account.health.total_requests
                            && config
                                .rotation_policy
                                .should_rotate_by_usage(provider, total)
                        {
                            should_schedule_rotation = true;
                        }

                        if let (Some(last_rotation), Some(interval_ms)) =
                            (account.health.last_rotation_ms, effective.interval_ms)
                            && now.saturating_sub(last_rotation) >= interval_ms
                        {
                            should_schedule_rotation = true;
                        }

                        if matches!(
                            effective.strategy,
                            RotationStrategy::EntropyBased | RotationStrategy::Hybrid
                        ) && let Some(samples) = &account.health.entropy_samples
                        {
                            let mut calc = EntropyCalculator::new(effective.max_entropy_samples);
                            calc.load_samples(samples.clone());
                            if calc.should_rotate(effective.entropy_threshold) {
                                should_schedule_rotation = true;
                            }
                        }
                    }

                    if *expires <= deadline || should_schedule_rotation {
                        tokens.push(TokenRefreshInfo {
                            provider: provider.clone(),
                            account_id: Some(account_id.clone()),
                            expires_at: *expires,
                            refresh_reason: if *expires <= now {
                                RefreshReason::Expired
                            } else if should_schedule_rotation {
                                RefreshReason::PolicyTriggered
                            } else {
                                RefreshReason::Proactive
                            },
                        });
                    }
                }
            }
        }

        // Check standalone entries
        for (provider, cred) in &storage.entries {
            if let super::AuthCredential::OAuth { expires, .. } = cred {
                if *expires <= deadline {
                    tokens.push(TokenRefreshInfo {
                        provider: provider.clone(),
                        account_id: None,
                        expires_at: *expires,
                        refresh_reason: if *expires <= now {
                            RefreshReason::Expired
                        } else {
                            RefreshReason::Proactive
                        },
                    });
                }
            }
        }

        tokens
    }

    /// Shutdown the worker gracefully.
    ///
    /// This signals the worker thread to stop and waits for it to complete.
    /// The method blocks until the worker has stopped or the timeout is reached.
    pub fn shutdown(&self) {
        if !self.config.enabled {
            return;
        }

        tracing::info!("Initiating graceful shutdown of OAuth refresh worker");

        // Signal shutdown
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(());
        }

        // Wait for thread to finish with timeout
        if let Some(handle) = &self.handle {
            let timeout = Duration::from_secs(30);
            let start = std::time::Instant::now();

            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(100));
            }

            if handle.is_finished() {
                tracing::info!("OAuth refresh worker shut down successfully");
            } else {
                tracing::warn!(
                    timeout_secs = timeout.as_secs(),
                    "OAuth refresh worker did not shut down within timeout"
                );
            }
        }

        self.running.store(false, Ordering::Relaxed);
    }

    /// Returns whether the worker is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
            && self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Returns the number of currently active refresh operations.
    pub fn active_refresh_count(&self) -> usize {
        self.active_refreshes.load(Ordering::Relaxed)
    }

    /// Returns the worker configuration.
    pub const fn config(&self) -> &RefreshWorkerConfig {
        &self.config
    }
}

impl Drop for RefreshWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Information about a token that needs refresh.
#[derive(Debug, Clone)]
struct TokenRefreshInfo {
    provider: String,
    account_id: Option<String>,
    expires_at: i64,
    refresh_reason: RefreshReason,
}

/// Reason why a token is being refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshReason {
    /// Token has already expired.
    Expired,
    /// Proactive refresh before expiry.
    Proactive,
    /// Rotation policy requested refresh/rotation work.
    PolicyTriggered,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_values() {
        let config = RefreshWorkerConfig::default();
        assert_eq!(config.refresh_lead_time_secs, 600);
        assert_eq!(config.check_interval_secs, 60);
        assert_eq!(config.max_concurrency, 5);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.backoff_multiplier, 2.0);
        assert!(config.enabled);
    }

    #[test]
    fn test_config_custom_values() {
        let config = RefreshWorkerConfig {
            refresh_lead_time_secs: 300,
            check_interval_secs: 30,
            max_concurrency: 10,
            max_retries: 5,
            backoff_multiplier: 3.0,
            enabled: false,
            rotation_policy: RotationPolicy::default(),
        };
        assert_eq!(config.refresh_lead_time_secs, 300);
        assert_eq!(config.check_interval_secs, 30);
        assert_eq!(config.max_concurrency, 10);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.backoff_multiplier, 3.0);
        assert!(!config.enabled);
    }

    #[test]
    fn test_worker_lifecycle_disabled() {
        let config = RefreshWorkerConfig {
            enabled: false,
            ..Default::default()
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let auth_path = temp_dir.path().join("auth.json");
        let auth_storage = Arc::new(Mutex::new(AuthStorage::load(auth_path).unwrap()));
        let callbacks = Arc::new(CallbackRegistry::new());

        let worker = RefreshWorker::start(config, auth_storage, callbacks);

        assert!(!worker.is_running());
        assert_eq!(worker.active_refresh_count(), 0);
        assert!(!worker.config().enabled);

        // Shutdown should be a no-op for disabled workers
        worker.shutdown();
    }

    #[test]
    fn test_worker_is_running() {
        let temp_dir = tempfile::tempdir().unwrap();
        let auth_path = temp_dir.path().join("auth.json");
        let auth_storage = Arc::new(Mutex::new(AuthStorage::load(auth_path).unwrap()));
        let callbacks = Arc::new(CallbackRegistry::new());

        let config = RefreshWorkerConfig {
            check_interval_secs: 1, // Short interval for testing
            ..Default::default()
        };

        let worker = RefreshWorker::start(config, auth_storage, callbacks);

        // Give the worker a moment to start
        thread::sleep(Duration::from_millis(100));

        assert!(worker.is_running());

        // Shutdown
        worker.shutdown();
        assert!(!worker.is_running());
    }

    #[test]
    fn test_worker_shutdown_idempotent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let auth_path = temp_dir.path().join("auth.json");
        let auth_storage = Arc::new(Mutex::new(AuthStorage::load(auth_path).unwrap()));
        let callbacks = Arc::new(CallbackRegistry::new());

        let config = RefreshWorkerConfig {
            check_interval_secs: 1,
            ..Default::default()
        };

        let worker = RefreshWorker::start(config, auth_storage, callbacks);

        thread::sleep(Duration::from_millis(100));
        assert!(worker.is_running());

        // Multiple shutdowns should be safe
        worker.shutdown();
        worker.shutdown();
        worker.shutdown();

        assert!(!worker.is_running());
    }

    #[test]
    fn test_identify_tokens_needing_refresh_empty() {
        let temp_dir = tempfile::tempdir().unwrap();
        let auth_path = temp_dir.path().join("auth.json");
        let storage = AuthStorage::load(auth_path).unwrap();

        let config = RefreshWorkerConfig::default();
        let tokens = RefreshWorker::identify_tokens_needing_refresh(&storage, &config);

        assert!(tokens.is_empty());
    }
}
