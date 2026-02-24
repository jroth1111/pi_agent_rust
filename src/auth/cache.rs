//! Credential cache for reducing I/O operations.
//!
//! This module provides an in-memory LRU cache for resolved credentials
//! with configurable TTL (time-to-live) to balance performance with
//! credential freshness.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default cache capacity (number of entries).
const DEFAULT_CAPACITY: usize = 64;

/// Default TTL for cached credentials (5 minutes).
const DEFAULT_TTL_SECS: u64 = 300;

/// A cached credential entry with expiration tracking.
///
/// This struct is returned by cache lookups and always contains a decrypted value.
#[derive(Debug, Clone)]
pub struct CachedEntry {
    /// The decrypted credential value.
    pub value: String,
    /// When this entry was cached.
    pub cached_at: Instant,
    /// Time-to-live for this entry.
    pub ttl: Duration,
    /// Provider this credential belongs to.
    pub provider: String,
    /// Optional account ID for OAuth credentials.
    pub account_id: Option<String>,
}

impl CachedEntry {
    /// Create a new cached entry.
    pub fn new(value: String, provider: String, ttl: Duration) -> Self {
        Self {
            value,
            cached_at: Instant::now(),
            ttl,
            provider,
            account_id: None,
        }
    }

    /// Create a new cached entry with account ID.
    pub fn with_account(
        value: String,
        provider: String,
        account_id: String,
        ttl: Duration,
    ) -> Self {
        Self {
            value,
            cached_at: Instant::now(),
            ttl,
            provider,
            account_id: Some(account_id),
        }
    }

    /// Check if this entry has expired.
    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }

    /// Get the remaining time until expiration.
    pub fn remaining_ttl(&self) -> Duration {
        self.ttl.saturating_sub(self.cached_at.elapsed())
    }
}

#[derive(Debug, Clone)]
struct EncryptedPayload {
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

#[derive(Debug, Clone)]
struct StoredCachedEntry {
    payload: EncryptedPayload,
    cached_at: Instant,
    ttl: Duration,
    provider: String,
    account_id: Option<String>,
}

impl StoredCachedEntry {
    fn new(
        payload: EncryptedPayload,
        provider: String,
        account_id: Option<String>,
        ttl: Duration,
    ) -> Self {
        Self {
            payload,
            cached_at: Instant::now(),
            ttl,
            provider,
            account_id,
        }
    }

    fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }

    fn into_decrypted(self, value: String) -> CachedEntry {
        CachedEntry {
            value,
            cached_at: self.cached_at,
            ttl: self.ttl,
            provider: self.provider,
            account_id: self.account_id,
        }
    }
}

/// Configuration for the credential cache.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of entries in the cache.
    pub capacity: usize,
    /// Default TTL for cached entries.
    pub default_ttl: Duration,
    /// Enable or disable the cache.
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
            default_ttl: Duration::from_secs(DEFAULT_TTL_SECS),
            enabled: true,
        }
    }
}

/// Thread-safe LRU cache for credentials.
#[derive(Debug)]
pub struct CredentialCache {
    /// The underlying LRU cache. Values are encrypted at rest in memory.
    cache: RwLock<LruCache<String, StoredCachedEntry>>,
    /// Cache configuration.
    config: CacheConfig,
    /// Process-local in-memory key used to encrypt cache entries.
    encryption_key: [u8; 32],
}

impl CredentialCache {
    /// Create a new credential cache with default configuration.
    pub fn new() -> Self {
        Self::with_config(CacheConfig::default())
    }

    /// Create a new credential cache with custom configuration.
    pub fn with_config(config: CacheConfig) -> Self {
        let capacity = NonZeroUsize::new(config.capacity.max(1)).unwrap();
        Self {
            cache: RwLock::new(LruCache::new(capacity)),
            config,
            encryption_key: Self::generate_process_local_key(),
        }
    }

    fn generate_process_local_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        if getrandom::fill(&mut key).is_ok() {
            return key;
        }

        // Fail-closed without panicking: derive a deterministic fallback from runtime entropy.
        let ts_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let material = format!(
            "pi-agent-rust-cache:{ts_nanos}:{:?}",
            std::thread::current().id()
        );
        let digest = Sha256::digest(material.as_bytes());
        let mut derived = [0u8; 32];
        derived.copy_from_slice(&digest[..]);
        tracing::warn!("Credential cache RNG unavailable; using derived fallback key");
        derived
    }

    fn encrypt_value(&self, value: &str) -> Option<EncryptedPayload> {
        let mut nonce = [0u8; 12];
        if getrandom::fill(&mut nonce).is_err() {
            tracing::warn!("Credential cache nonce generation failed; skipping cache insert");
            return None;
        }

        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key).ok()?;
        let nonce_value = Nonce::from(nonce);
        let ciphertext = cipher.encrypt(&nonce_value, value.as_bytes()).ok()?;

        Some(EncryptedPayload { nonce, ciphertext })
    }

    fn decrypt_value(&self, payload: &EncryptedPayload) -> Option<String> {
        let cipher = Aes256Gcm::new_from_slice(&self.encryption_key).ok()?;
        let nonce_value = Nonce::from(payload.nonce);
        let plaintext = cipher
            .decrypt(&nonce_value, payload.ciphertext.as_ref())
            .ok()?;
        String::from_utf8(plaintext).ok()
    }

    fn put_internal(
        &self,
        key: String,
        provider: &str,
        account_id: Option<&str>,
        value: String,
        ttl: Duration,
    ) {
        let Some(payload) = self.encrypt_value(&value) else {
            return;
        };

        let entry = StoredCachedEntry::new(
            payload,
            provider.to_string(),
            account_id.map(str::to_string),
            ttl,
        );

        if let Ok(mut cache) = self.cache.write() {
            cache.put(key, entry);
        }
    }

    fn get_decrypted_by_key(&self, key: &str) -> Option<CachedEntry> {
        let mut cache = self.cache.write().ok()?;
        let stored = cache.peek(key)?.clone();

        if stored.is_expired() {
            cache.pop(key);
            return None;
        }

        let Some(value) = self.decrypt_value(&stored.payload) else {
            tracing::warn!(
                provider = stored.provider,
                "Credential cache decrypt failed; invalidating entry"
            );
            cache.pop(key);
            return None;
        };

        Some(stored.into_decrypted(value))
    }

    /// Generate a cache key for a provider.
    pub fn key_for_provider(provider: &str) -> String {
        provider.to_lowercase()
    }

    /// Generate a cache key for a provider + account combination.
    pub fn key_for_account(provider: &str, account_id: &str) -> String {
        format!("{}:{}", provider.to_lowercase(), account_id)
    }

    /// Get a cached credential for a provider.
    pub fn get(&self, provider: &str) -> Option<CachedEntry> {
        if !self.config.enabled {
            return None;
        }

        let key = Self::key_for_provider(provider);
        self.get_decrypted_by_key(&key)
    }

    /// Get a cached credential for a provider + account combination.
    pub fn get_for_account(&self, provider: &str, account_id: &str) -> Option<CachedEntry> {
        if !self.config.enabled {
            return None;
        }

        let key = Self::key_for_account(provider, account_id);
        self.get_decrypted_by_key(&key)
    }

    /// Store a credential in the cache.
    pub fn put(&self, provider: &str, value: String) {
        if !self.config.enabled {
            return;
        }

        let key = Self::key_for_provider(provider);
        self.put_internal(key, provider, None, value, self.config.default_ttl);
    }

    /// Store a credential with account ID in the cache.
    pub fn put_for_account(&self, provider: &str, account_id: &str, value: String) {
        if !self.config.enabled {
            return;
        }

        let key = Self::key_for_account(provider, account_id);
        self.put_internal(
            key,
            provider,
            Some(account_id),
            value,
            self.config.default_ttl,
        );
    }

    /// Store a credential with custom TTL.
    pub fn put_with_ttl(&self, provider: &str, value: String, ttl: Duration) {
        if !self.config.enabled {
            return;
        }

        let key = Self::key_for_provider(provider);
        self.put_internal(key, provider, None, value, ttl);
    }

    /// Invalidate a cached credential for a provider.
    pub fn invalidate(&self, provider: &str) {
        let key = Self::key_for_provider(provider);
        if let Ok(mut cache) = self.cache.write() {
            cache.pop(&key);
        }
    }

    /// Invalidate a cached credential for a provider + account combination.
    pub fn invalidate_for_account(&self, provider: &str, account_id: &str) {
        let key = Self::key_for_account(provider, account_id);
        if let Ok(mut cache) = self.cache.write() {
            cache.pop(&key);
        }
    }

    /// Invalidate cache entries affected by a rotation event.
    ///
    /// If `account_id` is provided, only that account and provider root entries
    /// are invalidated. Otherwise, all entries for the provider are invalidated.
    pub fn invalidate_for_rotation(&self, provider: &str, account_id: Option<&str>) {
        if let Some(account_id) = account_id {
            self.invalidate_for_account(provider, account_id);
            self.invalidate(provider);
            return;
        }
        self.invalidate_provider(provider);
    }

    /// Invalidate cache entries affected by an entropy-triggered rotation.
    pub fn invalidate_for_entropy_rotation(&self, provider: &str, account_id: &str) {
        self.invalidate_for_rotation(provider, Some(account_id));
    }

    /// Invalidate all cached credentials for a provider (including all accounts).
    pub fn invalidate_provider(&self, provider: &str) {
        let prefix = format!("{}:", provider.to_lowercase());
        if let Ok(mut cache) = self.cache.write() {
            // Collect keys to remove (can't modify during iteration)
            let keys_to_remove: Vec<String> = cache
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix) || k.eq_ignore_ascii_case(provider))
                .map(|(k, _)| k.clone())
                .collect();

            for key in keys_to_remove {
                cache.pop(&key);
            }
        }
    }

    /// Clear all cached credentials.
    pub fn clear(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    /// Get the number of entries in the cache (including expired).
    pub fn len(&self) -> usize {
        self.cache.read().map(|c| c.len()).unwrap_or(0)
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Purge expired entries from the cache.
    pub fn purge_expired(&self) {
        if let Ok(mut cache) = self.cache.write() {
            let keys_to_remove: Vec<String> = cache
                .iter()
                .filter(|(_, v)| v.is_expired())
                .map(|(k, _)| k.clone())
                .collect();

            for key in keys_to_remove {
                cache.pop(&key);
            }
        }
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        if let Ok(cache) = self.cache.read() {
            let total = cache.len();
            let expired = cache.iter().filter(|(_, v)| v.is_expired()).count();
            CacheStats {
                total_entries: total,
                expired_entries: expired,
                active_entries: total - expired,
                capacity: self.config.capacity,
            }
        } else {
            CacheStats::default()
        }
    }
}

impl Default for CredentialCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the cache state.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Total number of entries in the cache.
    pub total_entries: usize,
    /// Number of expired entries.
    pub expired_entries: usize,
    /// Number of active (non-expired) entries.
    pub active_entries: usize,
    /// Maximum capacity of the cache.
    pub capacity: usize,
}

/// Global credential cache instance.
static GLOBAL_CACHE: std::sync::OnceLock<Arc<CredentialCache>> = std::sync::OnceLock::new();

/// Get the global credential cache instance.
pub fn global_cache() -> Arc<CredentialCache> {
    GLOBAL_CACHE
        .get_or_init(|| Arc::new(CredentialCache::new()))
        .clone()
}

/// Initialize the global cache with custom configuration.
pub fn init_global_cache(config: CacheConfig) {
    let _ = GLOBAL_CACHE.get_or_init(|| Arc::new(CredentialCache::with_config(config)));
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cached_entry_expiry() {
        let entry = CachedEntry::new(
            "test-value".to_string(),
            "test-provider".to_string(),
            Duration::from_millis(10),
        );

        assert!(!entry.is_expired());

        std::thread::sleep(Duration::from_millis(20));

        assert!(entry.is_expired());
    }

    #[test]
    fn test_cache_put_get() {
        let cache = CredentialCache::new();

        cache.put("anthropic", "sk-test-123".to_string());

        let entry = cache.get("anthropic").expect("Should have entry");
        assert_eq!(entry.value, "sk-test-123");
    }

    #[test]
    fn test_cache_stores_encrypted_payload_not_plaintext() {
        let cache = CredentialCache::new();
        let secret = "sk-ant-secret-value-123";
        cache.put("anthropic", secret.to_string());

        let key = CredentialCache::key_for_provider("anthropic");
        let guard = cache.cache.read().expect("cache lock");
        let stored = guard.peek(&key).expect("stored entry");

        assert_ne!(
            stored.payload.ciphertext,
            secret.as_bytes(),
            "ciphertext should not equal plaintext bytes"
        );
        assert!(
            stored.payload.nonce.iter().any(|b| *b != 0),
            "nonce should be randomized and non-zero"
        );
    }

    #[test]
    fn test_cache_tamper_fails_closed_and_invalidates_entry() {
        let cache = CredentialCache::new();
        cache.put("anthropic", "sk-test-123".to_string());
        let key = CredentialCache::key_for_provider("anthropic");

        {
            let mut guard = cache.cache.write().expect("cache lock");
            let stored = guard.get_mut(&key).expect("stored entry");
            stored.payload.ciphertext[0] ^= 0xFF;
        }

        // Decrypt failure should return None and invalidate the entry.
        assert!(cache.get("anthropic").is_none());
        let guard = cache.cache.read().expect("cache lock");
        assert!(
            guard.peek(&key).is_none(),
            "tampered entry should be removed"
        );
    }

    #[test]
    fn test_cache_miss() {
        let cache = CredentialCache::new();

        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn test_cache_invalidation() {
        let cache = CredentialCache::new();

        cache.put("anthropic", "sk-test-123".to_string());
        assert!(cache.get("anthropic").is_some());

        cache.invalidate("anthropic");
        assert!(cache.get("anthropic").is_none());
    }

    #[test]
    fn test_cache_expiry() {
        let config = CacheConfig {
            default_ttl: Duration::from_millis(10),
            ..Default::default()
        };
        let cache = CredentialCache::with_config(config);

        cache.put("anthropic", "sk-test-123".to_string());

        // Should be present immediately
        assert!(cache.get("anthropic").is_some());

        // Wait for expiry
        std::thread::sleep(Duration::from_millis(20));

        // Should be expired and not returned
        assert!(cache.get("anthropic").is_none());
    }

    #[test]
    fn test_cache_account_key() {
        let cache = CredentialCache::new();

        cache.put_for_account("google", "user@example.com", "token-123".to_string());

        let entry = cache
            .get_for_account("google", "user@example.com")
            .expect("Should have entry");
        assert_eq!(entry.value, "token-123");
        assert_eq!(entry.account_id, Some("user@example.com".to_string()));
    }

    #[test]
    fn test_cache_provider_invalidation() {
        let cache = CredentialCache::new();

        cache.put("google", "main-key".to_string());
        cache.put_for_account("google", "user1@example.com", "token-1".to_string());
        cache.put_for_account("google", "user2@example.com", "token-2".to_string());

        cache.invalidate_provider("google");

        assert!(cache.get("google").is_none());
        assert!(
            cache
                .get_for_account("google", "user1@example.com")
                .is_none()
        );
        assert!(
            cache
                .get_for_account("google", "user2@example.com")
                .is_none()
        );
    }

    #[test]
    fn test_cache_stats() {
        let cache = CredentialCache::new();

        cache.put("provider1", "key1".to_string());
        cache.put("provider2", "key2".to_string());

        let stats = cache.stats();
        assert_eq!(stats.total_entries, 2);
        assert_eq!(stats.active_entries, 2);
        assert_eq!(stats.expired_entries, 0);
    }

    #[test]
    fn test_cache_clear() {
        let cache = CredentialCache::new();

        cache.put("provider1", "key1".to_string());
        cache.put("provider2", "key2".to_string());

        assert_eq!(cache.len(), 2);

        cache.clear();

        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_disabled() {
        let config = CacheConfig {
            enabled: false,
            ..Default::default()
        };
        let cache = CredentialCache::with_config(config);

        cache.put("anthropic", "sk-test-123".to_string());

        // Should not return anything when disabled
        assert!(cache.get("anthropic").is_none());
    }

    #[test]
    fn test_case_insensitive_lookup() {
        let cache = CredentialCache::new();

        cache.put("Anthropic", "sk-test-123".to_string());

        let entry = cache.get("anthropic").expect("Should have entry");
        assert_eq!(entry.value, "sk-test-123");

        let entry2 = cache.get("ANTHROPIC").expect("Should have entry");
        assert_eq!(entry2.value, "sk-test-123");
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    /// Strategy for generating valid provider names
    fn provider_name_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_-]{0,19}"
    }

    /// Strategy for generating token values
    fn token_value_strategy() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{10,50}"
    }

    /// Strategy for generating account IDs
    fn account_id_strategy() -> impl Strategy<Value = Option<String>> {
        prop_oneof![
            Just(None),
            "[a-z][a-z0-9._-]{5,30}@[a-z]{3,10}\\.[a-z]{2,5}".prop_map(Some)
        ]
    }

    proptest! {
        /// Test that cache keys are case-insensitive for providers
        #[test]
        fn cache_key_case_insensitive(
            provider in provider_name_strategy(),
            value in token_value_strategy()
        ) {
            let cache = CredentialCache::new();
            let expected_value = value.clone();

            // Store with original case
            cache.put(&provider, value);

            // Retrieve with uppercase
            let upper = provider.to_uppercase();
            let entry = cache.get(&upper);
            prop_assert!(entry.is_some());
            prop_assert_eq!(entry.unwrap().value, expected_value.clone());

            // Retrieve with mixed case
            let mixed: String = provider.chars().enumerate()
                .map(|(i, c)| if i % 2 == 0 { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
                .collect();
            let entry = cache.get(&mixed);
            prop_assert!(entry.is_some());
            prop_assert_eq!(entry.unwrap().value, expected_value.clone());
        }

        /// Test that cache invalidation removes entries
        #[test]
        fn cache_invalidation_removes_entry(
            provider in provider_name_strategy(),
            value in token_value_strategy()
        ) {
            let cache = CredentialCache::new();

            cache.put(&provider, value);
            prop_assert!(cache.get(&provider).is_some());

            cache.invalidate(&provider);
            prop_assert!(cache.get(&provider).is_none());
        }

        /// Test that provider invalidation removes all account entries
        #[test]
        fn provider_invalidation_removes_accounts(
            provider in provider_name_strategy(),
            account1 in account_id_strategy(),
            account2 in account_id_strategy(),
            value1 in token_value_strategy(),
            value2 in token_value_strategy()
        ) {
            // Skip if accounts are the same or both None
            prop_assume!(account1 != account2);

            let cache = CredentialCache::new();

            // Store main provider entry
            cache.put(&provider, value1.clone());

            // Store account entries if accounts provided
            if let (Some(ref acc1), Some(ref acc2)) = (account1, account2) {
                cache.put_for_account(&provider, acc1, value1.clone());
                cache.put_for_account(&provider, acc2, value2.clone());

                // Verify all entries exist
                prop_assert!(cache.get(&provider).is_some());
                prop_assert!(cache.get_for_account(&provider, acc1).is_some());
                prop_assert!(cache.get_for_account(&provider, acc2).is_some());

                // Invalidate provider
                cache.invalidate_provider(&provider);

                // All entries should be gone
                prop_assert!(cache.get(&provider).is_none());
                prop_assert!(cache.get_for_account(&provider, acc1).is_none());
                prop_assert!(cache.get_for_account(&provider, acc2).is_none());
            }
        }

        /// Test that cache respects capacity limits with LRU eviction
        #[test]
        fn cache_respects_capacity_with_lru_eviction(
            providers in prop::collection::vec(provider_name_strategy(), 5..20)
        ) {
            // Create a small cache (capacity 5)
            let config = CacheConfig {
                capacity: 5,
                default_ttl: Duration::from_secs(300),
                enabled: true,
            };
            let cache = CredentialCache::with_config(config);

            // Store all providers
            for (i, provider) in providers.iter().enumerate() {
                cache.put(provider, format!("value-{}", i));
            }

            // Cache should not exceed capacity
            let stats = cache.stats();
            prop_assert!(stats.total_entries <= 5,
                "Cache capacity {} exceeded: {} entries",
                5, stats.total_entries);
        }

        /// Test that entry expiry is calculated correctly
        #[test]
        fn entry_expiry_calculation(
            value in token_value_strategy(),
            provider in provider_name_strategy(),
            ttl_secs in 1u64..3600
        ) {
            let entry = CachedEntry::new(
                value,
                provider,
                Duration::from_secs(ttl_secs),
            );

            // New entry should not be expired
            prop_assert!(!entry.is_expired());

            // Remaining TTL should be close to configured TTL
            let remaining = entry.remaining_ttl();
            prop_assert!(remaining <= Duration::from_secs(ttl_secs));
            prop_assert!(remaining > Duration::from_secs(ttl_secs.saturating_sub(1)));
        }

        /// Test that key_for_provider normalizes to lowercase
        #[test]
        fn key_for_provider_normalizes_case(provider in "[a-zA-Z][a-zA-Z0-9_-]*") {
            let key = CredentialCache::key_for_provider(&provider);
            prop_assert!(key.chars().all(|c| !c.is_ascii_uppercase()));
            prop_assert_eq!(key, provider.to_lowercase());
        }

        /// Test that key_for_account format is consistent
        #[test]
        fn key_for_account_format(
            provider in provider_name_strategy(),
            account_id in "[a-z][a-z0-9._-]*"
        ) {
            let key = CredentialCache::key_for_account(&provider, &account_id);
            prop_assert!(key.contains(':'));
            prop_assert!(key.starts_with(&provider.to_lowercase()));
            prop_assert!(key.ends_with(&account_id));
        }

        /// Test cache clear removes all entries
        #[test]
        fn cache_clear_removes_all(
            providers in prop::collection::vec(provider_name_strategy(), 1..10)
        ) {
            let cache = CredentialCache::new();

            for provider in &providers {
                cache.put(provider, "test-value".to_string());
            }

            prop_assert!(cache.len() > 0);

            cache.clear();

            prop_assert_eq!(cache.len(), 0);
            prop_assert!(cache.is_empty());
        }

        /// Test that disabled cache returns nothing
        #[test]
        fn disabled_cache_returns_none(
            provider in provider_name_strategy(),
            value in token_value_strategy()
        ) {
            let config = CacheConfig {
                enabled: false,
                ..Default::default()
            };
            let cache = CredentialCache::with_config(config);

            cache.put(&provider, value);

            prop_assert!(cache.get(&provider).is_none());
            prop_assert_eq!(cache.len(), 0);
        }

        /// Test purge_expired only removes expired entries
        #[test]
        fn purge_expired_behavior(
            providers in prop::collection::vec(provider_name_strategy(), 2..5)
        ) {
            let config = CacheConfig {
                default_ttl: Duration::from_millis(5),
                ..Default::default()
            };
            let cache = CredentialCache::with_config(config);

            // Store entries
            for provider in &providers {
                cache.put(provider, "value".to_string());
            }

            // All entries should be present
            let stats_before = cache.stats();
            let expected_entries = providers
                .iter()
                .map(|provider| provider.to_lowercase())
                .collect::<HashSet<_>>()
                .len();
            prop_assert_eq!(stats_before.total_entries, expected_entries);
            prop_assert_eq!(stats_before.active_entries, expected_entries);

            // Wait for expiry
            std::thread::sleep(Duration::from_millis(20));

            // Purge expired
            cache.purge_expired();

            // All entries should be removed
            let stats_after = cache.stats();
            prop_assert_eq!(stats_after.total_entries, 0);
        }
    }
}
