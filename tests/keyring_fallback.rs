//! Keyring fallback integration tests.
//!
//! These tests exercise production auth storage backends directly:
//! - file-only fallback roundtrip
//! - keyring-with-file-fallback synchronization semantics
//! - fallback read path when keyring has no entry
//! - keyring operations do not panic on headless/unavailable environments

use pi::auth::{AuthStorageBackend, StorageStrategy};

fn refresh_key(provider: &str) -> String {
    format!("refresh-token:{provider}")
}

#[test]
fn file_only_strategy_roundtrip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("tokens.json");
    let backend = AuthStorageBackend::new(StorageStrategy::FileOnly, None, Some(path.clone()));

    backend
        .set_refresh_token("anthropic", "refresh-token-value")
        .expect("store token");

    let value = backend.get_refresh_token("anthropic").expect("get token");
    assert_eq!(value.as_deref(), Some("refresh-token-value"));

    backend
        .delete_refresh_token("anthropic")
        .expect("delete token");
    assert_eq!(
        backend
            .get_refresh_token("anthropic")
            .expect("get after delete"),
        None
    );

    let content = std::fs::read_to_string(&path).expect("read fallback file");
    assert!(content.contains('{'));
}

#[test]
fn keyring_with_fallback_keeps_file_storage_in_sync() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("tokens.json");
    let provider = "anthropic-sync";
    let key = refresh_key(provider);

    let backend = AuthStorageBackend::new(
        StorageStrategy::KeyringWithFileFallback,
        Some("pi-agent-rust-tests-sync".to_string()),
        Some(path.clone()),
    );

    backend
        .set_refresh_token(provider, "sync-refresh-token")
        .expect("set refresh token with fallback strategy");

    // Even when keyring succeeds, fallback is updated in-sync.
    let file_text = std::fs::read_to_string(&path).expect("fallback file should exist");
    let parsed: serde_json::Value = serde_json::from_str(&file_text).expect("valid json");
    assert_eq!(
        parsed.get(&key).and_then(serde_json::Value::as_str),
        Some("sync-refresh-token")
    );

    let fetched = backend
        .get_refresh_token(provider)
        .expect("get refresh token");
    assert_eq!(fetched.as_deref(), Some("sync-refresh-token"));

    backend
        .delete_refresh_token(provider)
        .expect("delete token");

    let file_text = std::fs::read_to_string(&path).expect("fallback file should remain readable");
    let parsed: serde_json::Value = serde_json::from_str(&file_text).expect("valid json");
    assert!(parsed.get(&key).is_none());
}

#[test]
fn keyring_with_fallback_reads_file_when_primary_has_no_entry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("tokens.json");
    let provider = "anthropic-fallback-read";

    // Seed only fallback file with a token.
    let fallback_only =
        AuthStorageBackend::new(StorageStrategy::FileOnly, None, Some(path.clone()));
    fallback_only
        .set_refresh_token(provider, "fallback-value")
        .expect("seed fallback token");

    let backend = AuthStorageBackend::new(
        StorageStrategy::KeyringWithFileFallback,
        Some("pi-agent-rust-tests-fallback-read".to_string()),
        Some(path),
    );

    let fetched = backend
        .get_refresh_token(provider)
        .expect("fallback read should succeed");
    assert_eq!(fetched.as_deref(), Some("fallback-value"));
}

#[test]
fn keyring_only_operations_do_not_panic_on_unavailable_services() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("tokens.json");
    let backend = AuthStorageBackend::new(
        StorageStrategy::KeyringOnly,
        Some("pi-agent-rust-tests-no-panic".to_string()),
        Some(path),
    );

    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = backend.get_refresh_token("anthropic-no-panic");
        let _ = backend.set_refresh_token("anthropic-no-panic", "value");
        let _ = backend.delete_refresh_token("anthropic-no-panic");
    }));

    assert!(
        run.is_ok(),
        "keyring operations must return Result errors instead of panicking"
    );
}
