//! Credential cache security tests.
//!
//! These tests verify production cache behavior:
//! - cache stores encrypted payloads internally (no plaintext-at-rest debug leakage)
//! - get/put roundtrip returns decrypted values correctly
//! - explicit invalidation and clear-on-logout semantics work
//! - TTL expiration and purge semantics behave as expected

use std::time::Duration;

use pi::auth::cache::{CacheConfig, CredentialCache, global_cache, init_global_cache};

#[test]
fn test_cache_roundtrip_returns_decrypted_value() {
    let cache = CredentialCache::new();
    cache.put("anthropic", "sk-ant-test-key".to_string());

    let entry = cache.get("anthropic").expect("entry should exist");
    assert_eq!(entry.value, "sk-ant-test-key");
}

#[test]
fn test_cache_debug_output_does_not_leak_plaintext_token() {
    let cache = CredentialCache::new();
    let secret = "sk-ant-api03-very-sensitive-token";
    cache.put("anthropic", secret.to_string());

    let debug_output = format!("{cache:?}");
    assert!(
        !debug_output.contains(secret),
        "cache debug output should not contain plaintext credentials"
    );
}

#[test]
fn test_cache_clear_on_logout() {
    let cache = CredentialCache::new();

    cache.put("anthropic", "sk-ant-test-key".to_string());
    cache.put_for_account("google", "user@example.com", "oauth-token".to_string());

    assert!(cache.get("anthropic").is_some());
    assert!(
        cache
            .get_for_account("google", "user@example.com")
            .is_some()
    );

    cache.clear();

    assert!(cache.get("anthropic").is_none());
    assert!(
        cache
            .get_for_account("google", "user@example.com")
            .is_none()
    );
    assert_eq!(cache.len(), 0);
}

#[test]
fn test_cache_invalidate_provider_on_logout() {
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
fn test_cache_ttl_expiration_removes_entries_from_lookup() {
    let config = CacheConfig {
        default_ttl: Duration::from_millis(50),
        ..Default::default()
    };
    let cache = CredentialCache::with_config(config);

    cache.put("anthropic", "sk-test-key".to_string());
    assert!(cache.get("anthropic").is_some());

    std::thread::sleep(Duration::from_millis(60));

    assert!(
        cache.get("anthropic").is_none(),
        "expired entries should not be returned"
    );
}

#[test]
fn test_cache_purge_expired_removes_only_expired() {
    let config = CacheConfig {
        default_ttl: Duration::from_millis(100),
        ..Default::default()
    };
    let cache = CredentialCache::with_config(config);

    cache.put_with_ttl(
        "ephemeral",
        "short-lived".to_string(),
        Duration::from_millis(50),
    );
    cache.put_with_ttl(
        "persistent",
        "long-lived".to_string(),
        Duration::from_secs(10),
    );

    std::thread::sleep(Duration::from_millis(60));
    cache.purge_expired();

    let stats = cache.stats();
    assert_eq!(stats.active_entries, 1);
    assert_eq!(stats.expired_entries, 0);
    assert!(cache.get("ephemeral").is_none());
    assert_eq!(
        cache
            .get("persistent")
            .expect("persistent entry should remain")
            .value,
        "long-lived"
    );
}

#[test]
fn test_cache_account_roundtrip_preserves_account_id() {
    let cache = CredentialCache::new();
    cache.put_for_account(
        "google",
        "user@example.com",
        "oauth-refresh-token-secret".to_string(),
    );

    let entry = cache
        .get_for_account("google", "user@example.com")
        .expect("account entry should exist");

    assert_eq!(entry.value, "oauth-refresh-token-secret");
    assert_eq!(entry.account_id.as_deref(), Some("user@example.com"));
}

#[test]
fn test_cache_disabled_does_not_store_credentials() {
    let config = CacheConfig {
        enabled: false,
        ..Default::default()
    };
    let cache = CredentialCache::with_config(config);

    cache.put("anthropic", "sk-test-key".to_string());
    assert!(cache.get("anthropic").is_none());
}

#[test]
fn test_global_cache_initialization() {
    let config = CacheConfig {
        capacity: 128,
        default_ttl: Duration::from_secs(600),
        enabled: true,
    };

    init_global_cache(config);

    let global = global_cache();
    global.put("test-provider", "test-value".to_string());

    let entry = global.get("test-provider");
    assert!(entry.is_some());
    assert_eq!(entry.expect("entry").value, "test-value");
}

#[test]
fn test_cache_case_insensitive_key_generation() {
    let key1 = CredentialCache::key_for_provider("Anthropic");
    let key2 = CredentialCache::key_for_provider("anthropic");
    let key3 = CredentialCache::key_for_provider("ANTHROPIC");

    assert_eq!(key1, key2);
    assert_eq!(key2, key3);
}

#[test]
fn test_cache_account_key_generation() {
    let key1 = CredentialCache::key_for_account("google", "user@example.com");
    let key2 = CredentialCache::key_for_account("GOOGLE", "user@example.com");
    let key3 = CredentialCache::key_for_account("google", "USER@EXAMPLE.COM");

    assert_eq!(key1, key2);
    assert_ne!(key1, key3);
}
