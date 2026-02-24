//! Secure token storage using platform-native keyring.
//!
//! This module provides:
//! - `SecureStorage` trait for abstracting secure storage backends
//! - `KeyringStorage` using platform-native keyring (macOS Keychain, Windows Credential Manager, Linux secret-service)
//! - `FileStorage` fallback implementation
//! - `AuthStorageBackend` manager with keyring-primary, file-fallback strategy

use crate::error::{Error, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// Service name used for keyring entries.
const KEYRING_SERVICE: &str = "pi-agent";

/// Key prefix for storing refresh tokens in the keyring.
const REFRESH_TOKEN_KEY_PREFIX: &str = "refresh-token:";

/// Trait for secure storage of authentication tokens.
///
/// This trait abstracts the storage backend, allowing for different implementations
/// such as platform-native keyring or file-based storage.
pub trait SecureStorage: Send + Sync {
    /// Retrieve a value from secure storage.
    ///
    /// # Arguments
    /// * `key` - The unique identifier for the stored value
    fn get(&self, key: &str) -> Result<Option<String>>;

    /// Store a value in secure storage.
    ///
    /// # Arguments
    /// * `key` - The unique identifier for the value
    /// * `value` - The value to store
    fn set(&self, key: &str, value: &str) -> Result<()>;

    /// Delete a value from secure storage.
    ///
    /// # Arguments
    /// * `key` - The unique identifier for the value to delete
    fn delete(&self, key: &str) -> Result<()>;

    /// Check if this storage backend is available and functional.
    fn is_available(&self) -> bool;
}

/// Platform-native keyring storage implementation.
///
/// Uses the `keyring` crate to store tokens securely:
/// - macOS: Keychain
/// - Windows: Credential Manager
/// - Linux: secret-service (via libsecret)
#[derive(Debug, Clone)]
pub struct KeyringStorage {
    /// Username for keyring entries (used as additional identifier).
    username: String,
}

impl KeyringStorage {
    /// Create a new keyring storage backend.
    ///
    /// # Arguments
    /// * `username` - Username/identifier for keyring entries (defaults to "default" if empty)
    pub fn new(username: Option<String>) -> Self {
        Self {
            username: username.unwrap_or_else(|| "default".to_string()),
        }
    }

    /// Get the keyring entry for a given key.
    #[cfg(feature = "keyring")]
    fn get_entry(&self, key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("{}/{}", self.username, key)).map_err(|e| {
            tracing::debug!(
                key = Self::safe_key_display(key),
                error = %e,
                "keyring entry creation failed"
            );
            Error::auth(format!("Failed to create keyring entry: {e}"))
        })
    }

    /// Build a display-safe version of the key for error messages.
    ///
    /// This truncates sensitive tokens in error messages.
    fn safe_key_display(key: &str) -> String {
        if key.len() > 20 {
            format!("{}...", &key[..20])
        } else {
            key.to_string()
        }
    }
}

impl Default for KeyringStorage {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SecureStorage for KeyringStorage {
    fn get(&self, key: &str) -> Result<Option<String>> {
        #[cfg(feature = "keyring")]
        {
            let entry = self.get_entry(key)?;
            match entry.get_password() {
                Ok(password) => Ok(Some(password)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => {
                    tracing::debug!(
                        key = Self::safe_key_display(key),
                        error = %e,
                        "keyring get failed"
                    );
                    Err(Error::auth(format!("Failed to retrieve from keyring: {e}")))
                }
            }
        }

        #[cfg(not(feature = "keyring"))]
        {
            let _ = key;
            Err(Error::auth("keyring feature is not enabled".to_string()))
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        #[cfg(feature = "keyring")]
        {
            let entry = self.get_entry(key)?;
            entry.set_password(value).map_err(|e| {
                tracing::debug!(
                    key = Self::safe_key_display(key),
                    error = %e,
                    "keyring set failed"
                );
                Error::auth(format!("Failed to store in keyring: {e}"))
            })
        }

        #[cfg(not(feature = "keyring"))]
        {
            let _ = (key, value);
            Err(Error::auth("keyring feature is not enabled".to_string()))
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        #[cfg(feature = "keyring")]
        {
            let entry = self.get_entry(key)?;
            match entry.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => {
                    // Already deleted - treat as success
                    Ok(())
                }
                Err(e) => {
                    tracing::debug!(
                        key = Self::safe_key_display(key),
                        error = %e,
                        "keyring delete failed"
                    );
                    Err(Error::auth(format!("Failed to delete from keyring: {e}")))
                }
            }
        }

        #[cfg(not(feature = "keyring"))]
        {
            let _ = key;
            Err(Error::auth("keyring feature is not enabled".to_string()))
        }
    }

    fn is_available(&self) -> bool {
        #[cfg(feature = "keyring")]
        {
            // The keyring is available if the feature is enabled
            // Actual availability will be tested on first real operation
            true
        }

        #[cfg(not(feature = "keyring"))]
        {
            false
        }
    }
}

/// File-based storage fallback for when keyring is unavailable.
///
/// This implementation stores values in a JSON file with restricted permissions.
/// It should only be used as a fallback when keyring storage is not available.
#[derive(Debug, Clone)]
pub struct FileStorage {
    /// Path to the storage file.
    path: PathBuf,
}

impl FileStorage {
    /// Create a new file storage backend.
    ///
    /// # Arguments
    /// * `path` - Path to the storage file (parent directories will be created if needed)
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Ensure the parent directory exists and has appropriate permissions.
    fn ensure_parent(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::auth(format!("Failed to create storage directory: {e}")))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o700);
                std::fs::set_permissions(parent, perms).map_err(|e| {
                    Error::auth(format!("Failed to set directory permissions: {e}"))
                })?;
            }
        }
        Ok(())
    }

    /// Load the storage file.
    fn load_file(&self) -> Result<std::collections::HashMap<String, String>> {
        if !self.path.exists() {
            return Ok(std::collections::HashMap::new());
        }

        let content = std::fs::read_to_string(&self.path)
            .map_err(|e| Error::auth(format!("Failed to read storage file: {e}")))?;

        if content.trim().is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        serde_json::from_str(&content)
            .map_err(|e| Error::auth(format!("Failed to parse storage file: {e}")))
    }

    /// Write data to the storage file atomically.
    fn write_file(&self, data: &std::collections::HashMap<String, String>) -> Result<()> {
        self.ensure_parent()?;

        let content = serde_json::to_string_pretty(data)
            .map_err(|e| Error::auth(format!("Failed to serialize storage data: {e}")))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut options = std::fs::File::options();
            options.write(true).create(true).truncate(true).mode(0o600);
            let mut file = options.open(&self.path).map_err(|e| {
                Error::auth(format!("Failed to open storage file for writing: {e}"))
            })?;
            use std::io::Write;
            file.write_all(content.as_bytes())
                .map_err(|e| Error::auth(format!("Failed to write storage file: {e}")))?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&self.path, content)
                .map_err(|e| Error::auth(format!("Failed to write storage file: {e}")))?;
        }

        Ok(())
    }
}

impl SecureStorage for FileStorage {
    fn get(&self, key: &str) -> Result<Option<String>> {
        let data = self.load_file()?;
        Ok(data.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut data = self.load_file()?;
        data.insert(key.to_string(), value.to_string());
        self.write_file(&data)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let mut data = self.load_file()?;
        data.remove(key);
        self.write_file(&data)
    }

    fn is_available(&self) -> bool {
        // File storage is always available if we can write to the path
        self.ensure_parent().is_ok()
    }
}

/// Strategy for storage backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageStrategy {
    /// Use keyring only; fail if unavailable.
    KeyringOnly,
    /// Prefer keyring with file fallback.
    KeyringWithFileFallback,
    /// Use file storage only.
    FileOnly,
}

/// Manager for authentication token storage backends.
///
/// This provides a unified interface with automatic fallback behavior.
pub struct AuthStorageBackend {
    /// Primary storage backend.
    primary: Arc<dyn SecureStorage>,
    /// Fallback storage backend (if any).
    fallback: Option<Arc<dyn SecureStorage>>,
    /// Strategy for backend selection.
    strategy: StorageStrategy,
}

impl std::fmt::Debug for AuthStorageBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthStorageBackend")
            .field("strategy", &self.strategy)
            .field("has_fallback", &self.fallback.is_some())
            .finish()
    }
}

impl Clone for AuthStorageBackend {
    fn clone(&self) -> Self {
        Self {
            primary: Arc::clone(&self.primary),
            fallback: self.fallback.as_ref().map(Arc::clone),
            strategy: self.strategy,
        }
    }
}

impl AuthStorageBackend {
    fn default_fallback_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".pi/agent/tokens.json")
    }

    /// Create a new auth storage backend with the given strategy.
    ///
    /// # Arguments
    /// * `strategy` - The storage strategy to use
    /// * `keyring_username` - Username for keyring entries (optional)
    /// * `fallback_path` - Path for file fallback (optional, defaults to ~/.pi/agent/tokens.json)
    pub fn new(
        strategy: StorageStrategy,
        keyring_username: Option<String>,
        fallback_path: Option<PathBuf>,
    ) -> Self {
        let fallback_path = fallback_path.unwrap_or_else(Self::default_fallback_path);

        let keyring = Arc::new(KeyringStorage::new(keyring_username)) as Arc<dyn SecureStorage>;
        let file_storage = Arc::new(FileStorage::new(fallback_path)) as Arc<dyn SecureStorage>;

        match strategy {
            StorageStrategy::KeyringOnly => Self {
                primary: keyring,
                fallback: None,
                strategy,
            },
            StorageStrategy::KeyringWithFileFallback => Self {
                primary: keyring.clone(),
                fallback: Some(file_storage),
                strategy,
            },
            StorageStrategy::FileOnly => Self {
                primary: file_storage,
                fallback: None,
                strategy,
            },
        }
    }

    /// Create the default storage backend (keyring with file fallback).
    pub fn default_backend() -> Self {
        Self::new(StorageStrategy::KeyringWithFileFallback, None, None)
    }

    /// Check if the primary backend is available.
    pub fn is_primary_available(&self) -> bool {
        self.primary.is_available()
    }

    /// Get the current storage strategy.
    pub const fn strategy(&self) -> StorageStrategy {
        self.strategy
    }

    /// Retrieve a refresh token for the given provider.
    ///
    /// # Arguments
    /// * `provider` - The provider identifier (e.g., "anthropic", "openai")
    pub fn get_refresh_token(&self, provider: &str) -> Result<Option<String>> {
        let key = format!("{REFRESH_TOKEN_KEY_PREFIX}{provider}");

        match self.primary.get(&key) {
            Ok(Some(value)) => Ok(Some(value)),
            Ok(None) => {
                // Try fallback if available
                if let Some(fallback) = &self.fallback {
                    fallback.get(&key)
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                // If primary fails and we have a fallback, try it
                if let Some(fallback) = &self.fallback {
                    tracing::debug!(
                        provider,
                        error = %e,
                        "primary storage failed, trying fallback"
                    );
                    fallback.get(&key)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Store a refresh token for the given provider.
    ///
    /// # Arguments
    /// * `provider` - The provider identifier
    /// * `token` - The refresh token to store
    pub fn set_refresh_token(&self, provider: &str, token: &str) -> Result<()> {
        let key = format!("{REFRESH_TOKEN_KEY_PREFIX}{provider}");

        match self.primary.set(&key, token) {
            Ok(()) => {
                // If we have a fallback, keep it in sync
                if let Some(fallback) = &self.fallback {
                    let _ = fallback.set(&key, token);
                }
                Ok(())
            }
            Err(e) => {
                // If primary fails and we have a fallback, try it
                if let Some(fallback) = &self.fallback {
                    tracing::debug!(
                        provider,
                        error = %e,
                        "primary storage failed, storing to fallback"
                    );
                    fallback.set(&key, token)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Delete a refresh token for the given provider.
    ///
    /// # Arguments
    /// * `provider` - The provider identifier
    pub fn delete_refresh_token(&self, provider: &str) -> Result<()> {
        let key = format!("{REFRESH_TOKEN_KEY_PREFIX}{provider}");

        // Always try to delete from both backends
        let primary_result = self.primary.delete(&key);
        let fallback_result = self.fallback.as_ref().map(|f| f.delete(&key));

        // Return success if either operation succeeds
        match (primary_result, fallback_result) {
            (Ok(()), Some(Ok(()))) => Ok(()),
            (Ok(()), Some(Err(_))) | (Err(_), Some(Ok(()))) => Ok(()),
            (Ok(()), None) => Ok(()),
            (Err(e), None) => Err(e),
            (Err(e), Some(Err(_))) => Err(e),
        }
    }

    /// Check if keyring storage is available.
    pub fn is_keyring_available(&self) -> bool {
        matches!(
            self.strategy,
            StorageStrategy::KeyringOnly | StorageStrategy::KeyringWithFileFallback
        ) && self.is_primary_available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keyring_storage_safe_display() {
        assert_eq!(KeyringStorage::safe_key_display("short"), "short");
        assert_eq!(
            KeyringStorage::safe_key_display("this_is_a_very_long_key_that_exceeds_limit"),
            "this_is_a_very_long_..."
        );
    }

    #[test]
    fn test_storage_strategy_display() {
        let backend = AuthStorageBackend::default_backend();
        assert_eq!(backend.strategy(), StorageStrategy::KeyringWithFileFallback);
    }

    #[test]
    fn test_refresh_token_key_format() {
        let key = format!("{}{}", REFRESH_TOKEN_KEY_PREFIX, "anthropic");
        assert!(key.starts_with("refresh-token:"));
        assert!(key.ends_with("anthropic"));
    }

    #[test]
    fn test_file_storage_ensure_parent_in_tempdir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage_path = temp_dir.path().join("nested/dir/tokens.json");
        let storage = FileStorage::new(storage_path);

        // ensure_parent is called internally by set
        let result = storage.set("test-key", "test-value");
        assert!(result.is_ok());
        assert!(storage.path.parent().unwrap().exists());
    }

    #[test]
    fn test_file_storage_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage_path = temp_dir.path().join("tokens.json");
        let storage = FileStorage::new(storage_path);

        // Store a value
        storage.set("test-key", "secret-value").unwrap();

        // Retrieve it
        let value = storage.get("test-key").unwrap();
        assert_eq!(value, Some("secret-value".to_string()));

        // Delete it
        storage.delete("test-key").unwrap();

        // Should be gone
        let value = storage.get("test-key").unwrap();
        assert!(value.is_none());
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Strategy for generating valid storage keys
    fn storage_key_strategy() -> impl Strategy<Value = String> {
        "[a-zA-Z][a-zA-Z0-9_-]{0,30}"
    }

    /// Strategy for generating token values (arbitrary strings without control chars)
    fn token_value_strategy() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_.-]{10,100}"
    }

    /// Strategy for generating provider names
    fn provider_name_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9-]{0,19}"
    }

    proptest! {
        /// Test FileStorage set/get roundtrip preserves values
        #[test]
        fn file_storage_roundtrip(
            key in storage_key_strategy(),
            value in token_value_strategy()
        ) {
            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");
            let storage = FileStorage::new(storage_path);

            // Set should succeed
            let set_result = storage.set(&key, &value);
            prop_assert!(set_result.is_ok());

            // Get should return the same value
            let retrieved = storage.get(&key);
            prop_assert!(retrieved.is_ok());
            prop_assert_eq!(retrieved.unwrap(), Some(value));
        }

        /// Test FileStorage delete removes entries
        #[test]
        fn file_storage_delete_removes_entry(
            key in storage_key_strategy(),
            value in token_value_strategy()
        ) {
            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");
            let storage = FileStorage::new(storage_path);

            // Store a value
            storage.set(&key, &value).unwrap();
            prop_assert!(storage.get(&key).unwrap().is_some());

            // Delete it
            let delete_result = storage.delete(&key);
            prop_assert!(delete_result.is_ok());

            // Should be gone
            prop_assert!(storage.get(&key).unwrap().is_none());

            // Deleting non-existent key should succeed (idempotent)
            let delete_again = storage.delete(&key);
            prop_assert!(delete_again.is_ok());
        }

        /// Test FileStorage handles multiple keys independently
        #[test]
        fn file_storage_multiple_keys(
            keys in prop::collection::vec(storage_key_strategy(), 1..10),
            value in token_value_strategy()
        ) {
            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");
            let storage = FileStorage::new(storage_path);

            // Store all keys with same value
            for key in &keys {
                storage.set(key, &value).unwrap();
            }

            // All keys should be retrievable
            for key in &keys {
                let retrieved = storage.get(key).unwrap();
                prop_assert_eq!(retrieved, Some(value.clone()));
            }
        }

        /// Test refresh token key format
        #[test]
        fn refresh_token_key_format(provider in provider_name_strategy()) {
            let key = format!("{}{}", REFRESH_TOKEN_KEY_PREFIX, provider);
            prop_assert!(key.starts_with("refresh-token:"));
            prop_assert!(key.ends_with(&provider));
        }

        /// Test AuthStorageBackend refresh token operations
        #[test]
        fn auth_storage_backend_token_operations(
            provider in provider_name_strategy(),
            token in token_value_strategy()
        ) {
            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");

            // Use FileOnly strategy for deterministic testing
            let backend = AuthStorageBackend::new(
                StorageStrategy::FileOnly,
                None,
                Some(storage_path),
            );

            // Store refresh token
            let set_result = backend.set_refresh_token(&provider, &token);
            prop_assert!(set_result.is_ok());

            // Retrieve it
            let retrieved = backend.get_refresh_token(&provider);
            prop_assert!(retrieved.is_ok());
            prop_assert_eq!(retrieved.unwrap(), Some(token.clone()));

            // Delete it
            let delete_result = backend.delete_refresh_token(&provider);
            prop_assert!(delete_result.is_ok());

            // Should be gone
            let after_delete = backend.get_refresh_token(&provider);
            prop_assert!(after_delete.is_ok());
            prop_assert!(after_delete.unwrap().is_none());
        }

        /// Test that safe_key_display truncates long keys
        #[test]
        fn safe_key_display_truncation(key in "[a-zA-Z0-9]{30,100}") {
            let display = KeyringStorage::safe_key_display(&key);

            if key.len() > 20 {
                // Should be truncated with ellipsis
                prop_assert!(display.ends_with("..."));
                prop_assert!(display.len() < key.len());
                prop_assert!(display.starts_with(&key[..20]));
            } else {
                // Should be unchanged
                prop_assert_eq!(display, key);
            }
        }

        /// Test that safe_key_display preserves short keys
        #[test]
        fn safe_key_display_preserves_short_keys(key in "[a-zA-Z0-9]{1,20}") {
            let display = KeyringStorage::safe_key_display(&key);
            prop_assert_eq!(display, key);
        }

        /// Test FileStorage handles empty values
        #[test]
        fn file_storage_empty_value(key in storage_key_strategy()) {
            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");
            let storage = FileStorage::new(storage_path);

            // Store empty value
            storage.set(&key, "").unwrap();

            // Should retrieve empty string
            let retrieved = storage.get(&key).unwrap();
            prop_assert_eq!(retrieved, Some("".to_string()));
        }

        /// Test storage strategy selection
        #[test]
        fn storage_strategy_selection(
            strategy in prop::sample::select(&[
                StorageStrategy::KeyringOnly,
                StorageStrategy::KeyringWithFileFallback,
                StorageStrategy::FileOnly,
            ][..])
        ) {
            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");

            let backend = AuthStorageBackend::new(
                strategy,
                Some("test-user".to_string()),
                Some(storage_path),
            );

            prop_assert_eq!(backend.strategy(), strategy);
        }

        /// Test that missing keys return None
        #[test]
        fn missing_key_returns_none(key in storage_key_strategy()) {
            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");
            let storage = FileStorage::new(storage_path);

            // Key was never set
            let result = storage.get(&key).unwrap();
            prop_assert!(result.is_none());
        }

        /// Test value overwriting
        #[test]
        fn file_storage_overwrites(
            key in storage_key_strategy(),
            value1 in token_value_strategy(),
            value2 in token_value_strategy()
        ) {
            prop_assume!(value1 != value2);

            let temp_dir = tempfile::tempdir().unwrap();
            let storage_path = temp_dir.path().join("tokens.json");
            let storage = FileStorage::new(storage_path);

            // Store first value
            storage.set(&key, &value1).unwrap();
            prop_assert_eq!(storage.get(&key).unwrap(), Some(value1));

            // Overwrite with second value
            storage.set(&key, &value2).unwrap();
            prop_assert_eq!(storage.get(&key).unwrap(), Some(value2));
        }
    }
}
