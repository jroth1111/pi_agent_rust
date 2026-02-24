//! Tests for OAuth schema/keyring integration and enhanced health tracking.

use pi::auth::keyring::{AuthStorageBackend, FileStorage, SecureStorage, StorageStrategy};
use pi::auth::{
    AUTH_SCHEMA_VERSION, AuthFile, AuthStorage, OAuthAccountHealth, OAuthAccountPolicy,
    OAuthAccountRecord, OAuthErrorClass, OAuthOutcome, OAuthProviderPool, RecoveryConfig,
    RecoveryContext, RecoveryEvent, RecoveryMachine, RecoveryMethod, RecoveryResult,
    RevocationManager, RevocationReason, parse_retry_after_header_enhanced,
};
use std::collections::HashMap;
use tempfile::TempDir;

// =============================================================================
// Schema Tests
// =============================================================================

#[test]
fn test_auth_schema_version_is_v3() {
    assert_eq!(AUTH_SCHEMA_VERSION, 3);
}

#[test]
fn test_auth_file_has_version_field() {
    let auth_file = AuthFile {
        version: AUTH_SCHEMA_VERSION,
        oauth_pools: HashMap::new(),
        entries: HashMap::new(),
    };

    let json = serde_json::to_string(&auth_file).unwrap();
    // Check version is present (serde may serialize without spaces)
    assert!(json.contains("\"version\":3") || json.contains("\"version\": 3"));
}

#[test]
fn test_retry_after_enhanced_numeric_seconds() {
    assert_eq!(parse_retry_after_header_enhanced("120", 0), Some(120_000));
}

#[test]
fn test_retry_after_enhanced_http_date_rfc7231() {
    let now = chrono::DateTime::parse_from_rfc3339("2015-10-21T07:27:00Z")
        .unwrap()
        .timestamp_millis();
    let retry = parse_retry_after_header_enhanced("Wed, 21 Oct 2015 07:28:00 GMT", now);
    assert_eq!(retry, Some(60_000));
}

#[test]
fn test_retry_after_enhanced_rfc2822() {
    let now = chrono::DateTime::parse_from_rfc3339("2015-10-21T07:27:30Z")
        .unwrap()
        .timestamp_millis();
    let retry = parse_retry_after_header_enhanced("Wed, 21 Oct 2015 07:28:00 +0000", now);
    assert_eq!(retry, Some(30_000));
}

#[test]
fn test_retry_after_enhanced_iso8601() {
    let now = chrono::DateTime::parse_from_rfc3339("2015-10-21T07:26:00Z")
        .unwrap()
        .timestamp_millis();
    let retry = parse_retry_after_header_enhanced("2015-10-21T07:28:00Z", now);
    assert_eq!(retry, Some(120_000));
}

#[test]
fn test_retry_after_enhanced_relative_units() {
    assert_eq!(
        parse_retry_after_header_enhanced("120 seconds", 0),
        Some(120_000)
    );
    assert_eq!(
        parse_retry_after_header_enhanced("2 minutes", 0),
        Some(120_000)
    );
}

#[test]
fn test_recovery_machine_reload_success_path() {
    let context = RecoveryContext::new("anthropic".to_string(), Some("acct-1".to_string()), 1000);
    let config = RecoveryConfig::default();
    let mut machine = RecoveryMachine::new(context, config);

    let started = machine.start(1000);
    assert!(matches!(started, RecoveryResult::InProgress { .. }));

    let done = machine.process_event(RecoveryEvent::ReloadComplete { success: true }, 1100);
    assert!(matches!(
        done,
        RecoveryResult::Success {
            method: RecoveryMethod::StorageReload,
            ..
        }
    ));
}

#[test]
fn test_recovery_machine_refresh_relogin_path() {
    let context = RecoveryContext::new(
        "openai-codex".to_string(),
        Some("acct-refresh".to_string()),
        1000,
    );
    let config = RecoveryConfig::default();
    let mut machine = RecoveryMachine::new(context, config);

    let _ = machine.start(1000);
    let _ = machine.process_event(RecoveryEvent::ReloadComplete { success: false }, 1100);
    let relogin = machine.process_event(
        RecoveryEvent::RefreshComplete {
            result: pi::auth::RefreshResult::RequiresRelogin,
        },
        1200,
    );
    assert!(matches!(relogin, RecoveryResult::RequiresUserAction { .. }));

    let done = machine.process_event(RecoveryEvent::ReloginComplete, 1300);
    assert!(matches!(
        done,
        RecoveryResult::Success {
            method: RecoveryMethod::ManualRelogin,
            ..
        }
    ));
}

#[test]
fn test_entropy_calculation_and_threshold_triggering() {
    let mut calc = pi::auth::EntropyCalculator::new(100);

    for i in 0..20 {
        calc.record_success(1000 + i64::from(i * 100), 10);
    }

    let entropy = calc.calculate();
    assert!(entropy <= 0.01, "expected low entropy, got {entropy}");
    assert!(calc.should_rotate(0.3));
}

#[test]
fn test_revocation_manager_add_check_and_clear() {
    let mut manager = RevocationManager::new();
    let token = "tok_abc123";

    manager.revoke(
        token,
        "anthropic".to_string(),
        Some("acct-1".to_string()),
        1_000,
        RevocationReason::UserRequested,
    );
    assert!(manager.is_revoked(token));
    assert_eq!(manager.revocation_log().len(), 1);

    manager.clear_revoked();
    assert!(!manager.is_revoked(token));
    assert_eq!(manager.revocation_log().len(), 1);
}

#[test]
fn test_auth_file_serialization_roundtrip() {
    let original = AuthFile {
        version: AUTH_SCHEMA_VERSION,
        oauth_pools: HashMap::new(),
        entries: HashMap::new(),
    };

    let json = serde_json::to_string(&original).unwrap();
    let deserialized: AuthFile = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.version, AUTH_SCHEMA_VERSION);
}

// =============================================================================
// Enhanced Health Tracking Tests
// =============================================================================

#[test]
fn test_oauth_account_health_v2_fields() {
    let health = OAuthAccountHealth {
        consecutive_failures: 2,
        cooldown_until_ms: Some(1000),
        requires_relogin: false,
        last_success_ms: Some(500),
        last_failure_ms: Some(900),
        last_error: Some("test error".to_string()),
        last_error_kind: Some(OAuthErrorClass::RateLimit),
        last_used_at_ms: Some(800),
        non_refreshable: false,
        requires_relogin_on_expiry: false,
        // V2 fields
        consecutive_successes: Some(5),
        last_status_code: Some(429),
        exhausted_until_ms: None,
        total_requests: Some(100),
        // V3 fields
        entropy_samples: None,
        last_rotation_ms: None,
        last_rotation_reason: None,
        recovery_state: None,
    };

    assert_eq!(health.consecutive_successes, Some(5));
    assert_eq!(health.last_status_code, Some(429));
    assert_eq!(health.exhausted_until_ms, None);
    assert_eq!(health.total_requests, Some(100));
}

#[test]
fn test_oauth_account_health_serialization_roundtrip() {
    let health = OAuthAccountHealth {
        consecutive_failures: 1,
        cooldown_until_ms: None,
        requires_relogin: false,
        last_success_ms: Some(500),
        last_failure_ms: None,
        last_error: None,
        last_error_kind: None,
        last_used_at_ms: None,
        non_refreshable: false,
        requires_relogin_on_expiry: false,
        consecutive_successes: Some(3),
        last_status_code: Some(200),
        exhausted_until_ms: Some(999_999),
        total_requests: Some(50),
        // V3 fields
        entropy_samples: None,
        last_rotation_ms: None,
        last_rotation_reason: None,
        recovery_state: None,
    };

    let json = serde_json::to_string(&health).unwrap();
    let deserialized: OAuthAccountHealth = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.consecutive_successes, Some(3));
    assert_eq!(deserialized.last_status_code, Some(200));
    assert_eq!(deserialized.exhausted_until_ms, Some(999_999));
    assert_eq!(deserialized.total_requests, Some(50));
}

#[test]
fn test_oauth_account_health_optional_fields_not_serialized_when_none() {
    let health = OAuthAccountHealth {
        consecutive_failures: 0,
        cooldown_until_ms: None,
        requires_relogin: false,
        last_success_ms: None,
        last_failure_ms: None,
        last_error: None,
        last_error_kind: None,
        last_used_at_ms: None,
        non_refreshable: false,
        requires_relogin_on_expiry: false,
        consecutive_successes: None,
        last_status_code: None,
        exhausted_until_ms: None,
        total_requests: None,
        // V3 fields
        entropy_samples: None,
        last_rotation_ms: None,
        last_rotation_reason: None,
        recovery_state: None,
    };

    let json = serde_json::to_string(&health).unwrap();

    // Optional fields should not appear in JSON when None
    assert!(!json.contains("consecutive_successes"));
    assert!(!json.contains("last_status_code"));
    assert!(!json.contains("exhausted_until_ms"));
    assert!(!json.contains("total_requests"));
}

// =============================================================================
// OAuth Outcome Tests
// =============================================================================

#[test]
fn test_oauth_outcome_with_status_code() {
    let outcome = OAuthOutcome {
        success: false,
        class: Some(OAuthErrorClass::RateLimit),
        retry_after_ms: Some(60000),
        reason: Some("Rate limited".to_string()),
        status_code: Some(429),
    };

    let json = serde_json::to_string(&outcome).unwrap();
    let deserialized: OAuthOutcome = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.status_code, Some(429));
}

// =============================================================================
// Keyring Integration Tests
// =============================================================================

#[test]
fn test_file_storage_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let storage_path = temp_dir.path().join("tokens.json");
    let storage = FileStorage::new(storage_path);

    // Test set
    storage.set("test-key", "test-value").unwrap();

    // Test get
    let value = storage.get("test-key").unwrap();
    assert_eq!(value, Some("test-value".to_string()));

    // Test delete
    storage.delete("test-key").unwrap();
    let value = storage.get("test-key").unwrap();
    assert_eq!(value, None);
}

#[test]
fn test_file_storage_is_available() {
    let temp_dir = TempDir::new().unwrap();
    let storage_path = temp_dir.path().join("tokens.json");
    let storage = FileStorage::new(storage_path);

    assert!(storage.is_available());
}

#[test]
fn test_auth_storage_backend_file_only_strategy() {
    let temp_dir = TempDir::new().unwrap();
    let storage_path = temp_dir.path().join("tokens.json");

    let backend = AuthStorageBackend::new(StorageStrategy::FileOnly, None, Some(storage_path));

    // Test refresh token operations
    backend
        .set_refresh_token("anthropic", "test-refresh-token")
        .unwrap();
    let token = backend.get_refresh_token("anthropic").unwrap();
    assert_eq!(token, Some("test-refresh-token".to_string()));

    backend.delete_refresh_token("anthropic").unwrap();
    let token = backend.get_refresh_token("anthropic").unwrap();
    assert_eq!(token, None);
}

#[test]
fn test_storage_strategy_enum() {
    assert_eq!(StorageStrategy::KeyringOnly, StorageStrategy::KeyringOnly);
    assert_eq!(
        StorageStrategy::KeyringWithFileFallback,
        StorageStrategy::KeyringWithFileFallback
    );
    assert_eq!(StorageStrategy::FileOnly, StorageStrategy::FileOnly);

    assert_ne!(StorageStrategy::KeyringOnly, StorageStrategy::FileOnly);
}

// =============================================================================
// OAuth Account Record Tests
// =============================================================================

#[test]
fn test_oauth_account_record_with_v2_health() {
    let record = OAuthAccountRecord {
        id: "test-account-1".to_string(),
        label: Some("Test Account".to_string()),
        credential: pi::auth::AuthCredential::ApiKey {
            key: "test-key".to_string(),
        },
        health: OAuthAccountHealth {
            consecutive_failures: 0,
            cooldown_until_ms: None,
            requires_relogin: false,
            last_success_ms: Some(1000),
            last_failure_ms: None,
            last_error: None,
            last_error_kind: None,
            last_used_at_ms: Some(900),
            non_refreshable: false,
            requires_relogin_on_expiry: false,
            consecutive_successes: Some(10),
            last_status_code: Some(200),
            exhausted_until_ms: None,
            total_requests: Some(1000),
            // V3 fields
            entropy_samples: None,
            last_rotation_ms: None,
            last_rotation_reason: None,
            recovery_state: None,
        },
        policy: OAuthAccountPolicy::default(),
        provider_metadata: HashMap::new(),
    };

    assert!(record.health.consecutive_successes.is_some());
    assert!(record.health.total_requests.is_some());
}

// =============================================================================
// OAuth Provider Pool Tests
// =============================================================================

#[test]
fn test_oauth_provider_pool_with_accounts() {
    let mut pool = OAuthProviderPool::default();

    let account = OAuthAccountRecord {
        id: "account-1".to_string(),
        label: None,
        credential: pi::auth::AuthCredential::ApiKey {
            key: "test-key".to_string(),
        },
        health: OAuthAccountHealth::default(),
        policy: OAuthAccountPolicy::default(),
        provider_metadata: HashMap::new(),
    };

    pool.accounts.insert("account-1".to_string(), account);
    pool.order.push("account-1".to_string());
    pool.active = Some("account-1".to_string());

    assert_eq!(pool.active, Some("account-1".to_string()));
    assert!(pool.accounts.contains_key("account-1"));
    assert_eq!(pool.order.len(), 1);
}

// =============================================================================
// Integration Tests (using AuthStorage)
// =============================================================================

#[test]
fn test_auth_storage_format_accounts_status() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");

    // Create a minimal auth storage
    let auth_file = AuthFile {
        version: AUTH_SCHEMA_VERSION,
        oauth_pools: HashMap::new(),
        entries: HashMap::new(),
    };

    // Write the auth file
    let json = serde_json::to_string_pretty(&auth_file).unwrap();
    std::fs::write(&auth_path, json).unwrap();

    // Load it
    let auth = AuthStorage::load(auth_path).unwrap();

    // Test format_accounts_status for a provider with no pool
    let status = auth.format_accounts_status("nonexistent");
    assert!(status.contains("No OAuth pool"));
}

#[test]
fn test_auth_storage_set_active_account() {
    let temp_dir = TempDir::new().unwrap();
    let auth_path = temp_dir.path().join("auth.json");

    // Create auth file with a pool
    let mut pool = OAuthProviderPool::default();
    let account = OAuthAccountRecord {
        id: "test-account".to_string(),
        label: Some("Test".to_string()),
        credential: pi::auth::AuthCredential::ApiKey {
            key: "test-key".to_string(),
        },
        health: OAuthAccountHealth::default(),
        policy: OAuthAccountPolicy::default(),
        provider_metadata: HashMap::new(),
    };
    pool.accounts.insert("test-account".to_string(), account);
    pool.order.push("test-account".to_string());

    let mut oauth_pools = HashMap::new();
    oauth_pools.insert("anthropic".to_string(), pool);

    let auth_file = AuthFile {
        version: AUTH_SCHEMA_VERSION,
        oauth_pools,
        entries: HashMap::new(),
    };

    let json = serde_json::to_string_pretty(&auth_file).unwrap();
    std::fs::write(&auth_path, json).unwrap();

    // Load and test
    let mut auth = AuthStorage::load(auth_path).unwrap();

    // Set active account
    let result = auth.set_active_account("anthropic", "test-account");
    assert!(result.is_ok());

    // Try to set nonexistent account
    let result = auth.set_active_account("anthropic", "nonexistent");
    assert!(result.is_err());

    // Try to set account for nonexistent provider
    let result = auth.set_active_account("nonexistent", "test-account");
    assert!(result.is_err());
}
