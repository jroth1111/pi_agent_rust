//! OAuth token redaction tests.
//!
//! These tests are intentionally wired to production redaction helpers from
//! `src/auth/mod.rs` to avoid drift between test-only logic and runtime behavior.
//!
//! Suite structure:
//!   - Unit tests (existing): verify `redact_known_secrets_for_tests` and
//!     `redact_sensitive_json_value_for_tests` helper functions.
//!   - Integration tests: verify that production `tracing::warn!` / `tracing::error!`
//!     calls on auth error paths never emit raw token values into log output.

mod common;

use pi::auth::{
    AuthStorage, redact_known_secrets_for_tests, redact_sensitive_json_value_for_tests,
};
use pi::models::OAuthConfig;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

const REDACTED: &str = "[REDACTED]";

#[test]
fn redacts_known_secret_tokens_from_plain_text() {
    let text = "oauth response access_token=abc123 refresh_token=def456";
    let redacted = redact_known_secrets_for_tests(text, &["abc123", "def456"]);

    assert!(!redacted.contains("abc123"));
    assert!(!redacted.contains("def456"));
    assert!(redacted.contains(REDACTED));
}

#[test]
fn redacts_sensitive_json_fields_recursively() {
    let mut payload = serde_json::json!({
        "access_token": "sk-ant-secret",
        "refresh_token": "refresh-secret",
        "nested": {
            "authorization": "Bearer top-secret",
            "safe": "ok"
        },
        "items": [
            {"id_token": "id-secret"},
            {"api_key": "api-secret"}
        ]
    });

    redact_sensitive_json_value_for_tests(&mut payload);

    assert_eq!(payload["access_token"].as_str(), Some(REDACTED));
    assert_eq!(payload["refresh_token"].as_str(), Some(REDACTED));
    assert_eq!(payload["nested"]["authorization"].as_str(), Some(REDACTED));
    assert_eq!(payload["nested"]["safe"].as_str(), Some("ok"));
    assert_eq!(payload["items"][0]["id_token"].as_str(), Some(REDACTED));
    assert_eq!(payload["items"][1]["api_key"].as_str(), Some(REDACTED));
}

#[test]
fn redacts_json_string_payload_without_explicit_secret_list() {
    let json_text = r#"{
        "event":"oauth_token_received",
        "access_token":"sk-ant-runtime-secret",
        "refresh_token":"refresh-runtime-secret",
        "safe":"ok"
    }"#;

    let redacted = redact_known_secrets_for_tests(json_text, &[]);

    assert!(redacted.contains("\"access_token\":\"[REDACTED]\""));
    assert!(redacted.contains("\"refresh_token\":\"[REDACTED]\""));
    assert!(redacted.contains("\"safe\":\"ok\""));
    assert!(!redacted.contains("runtime-secret"));
}

#[test]
fn redacts_authorization_header_value() {
    let mut request = serde_json::json!({
        "headers": {
            "Authorization": "Bearer sk-ant-header-secret",
            "Content-Type": "application/json"
        }
    });

    redact_sensitive_json_value_for_tests(&mut request);

    assert_eq!(request["headers"]["Authorization"].as_str(), Some(REDACTED));
    assert_eq!(
        request["headers"]["Content-Type"].as_str(),
        Some("application/json")
    );
}

#[test]
fn redaction_preserves_non_sensitive_context() {
    let text = "request failed for tenant=prod, token=abc123, retry=true";
    let redacted = redact_known_secrets_for_tests(text, &["abc123"]);

    assert!(redacted.contains("tenant=prod"));
    assert!(redacted.contains("retry=true"));
    assert!(!redacted.contains("abc123"));
}

// ── Integration tests: production tracing redaction ──────────────────────

/// A tracing layer that captures all formatted log output to a shared buffer.
struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl Clone for CapturingWriter {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CapturingWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// Spawn a minimal TCP server that returns one HTTP response and shuts down.
fn spawn_json_server(status_code: u16, body: &str) -> String {
    use std::io::Read as _;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let body = body.to_string();

    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");

        let mut chunk = [0_u8; 4096];
        let _ = socket.read(&mut chunk);

        let reason = match status_code {
            401 => "Unauthorized",
            500 => "Internal Server Error",
            _ => "OK",
        };
        let response = format!(
            "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .expect("write response");
        socket.flush().expect("flush response");
    });

    format!("http://{addr}/token")
}

/// Create an auth.json with an expired OAuth credential via the production
/// save path (avoiding serde format fragility).
fn create_auth_with_expired_oauth(
    dir: &std::path::Path,
    provider: &str,
    access_token: &str,
    refresh_token: &str,
    token_url: Option<String>,
    client_id: Option<String>,
) -> std::path::PathBuf {
    use pi::auth::AuthCredential;

    let path = dir.join("auth.json");
    let mut auth = AuthStorage::load(path.clone()).expect("create empty auth");
    auth.set(
        provider,
        AuthCredential::OAuth {
            access_token: access_token.to_string(),
            refresh_token: refresh_token.to_string(),
            expires: 0,
            token_url,
            client_id,
        },
    );
    auth.save().expect("save auth");
    path
}

/// Assert that a string contains no occurrences of any of the given secrets.
fn assert_no_leaked_secrets(text: &str, secrets: &[&str], context: &str) {
    for secret in secrets {
        assert!(
            !text.contains(secret),
            "{context}: raw secret {secret:?} found in output:\n{text}"
        );
    }
}

/// Verify that `refresh_expired_extension_oauth_tokens` redacts tokens in
/// tracing output when the token endpoint returns an error with echoed secrets.
#[test]
#[cfg(unix)]
fn tracing_output_redacts_tokens_on_extension_refresh_failure() {
    let access_token = "ACCESS-TOKEN-LEAKED-ef9a3c";
    let refresh_token = "REFRESH-TOKEN-LEAKED-b7d142";

    // Mock server echoes secrets in the 401 error body.
    let token_url = spawn_json_server(
        401,
        &format!(
            r#"{{"error":"invalid_grant","echo":"{refresh_token}","access_token":"{access_token}"}}"#,
        ),
    );

    let dir = tempfile::tempdir().expect("tmpdir");
    let auth_path = create_auth_with_expired_oauth(
        dir.path(),
        "test-ext",
        access_token,
        refresh_token,
        None,
        None,
    );

    let config = OAuthConfig {
        auth_url: String::new(),
        token_url,
        client_id: "test-client".to_string(),
        scopes: vec![],
        redirect_uri: None,
    };
    let mut extension_configs = HashMap::new();
    extension_configs.insert("test-ext".to_string(), config);

    // Install a tracing subscriber that captures all output.
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturingWriter(Arc::clone(&buffer));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(writer)
        .with_ansi(false)
        .finish();

    let result = tracing::subscriber::with_default(subscriber, || {
        common::run_async(async move {
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            let client = pi::http::client::Client::new();
            auth.refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await
        })
    });

    // The refresh should fail (401).
    assert!(result.is_err(), "expected refresh failure, got: {result:?}");

    // Verify the error message itself contains no raw tokens.
    let err_text = result.unwrap_err().to_string();
    assert_no_leaked_secrets(&err_text, &[access_token, refresh_token], "error Display");

    // Verify captured tracing output contains no raw tokens.
    let log_output = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert_no_leaked_secrets(
        &log_output,
        &[access_token, refresh_token],
        "tracing output",
    );
}

/// Verify that `refresh_expired_oauth_tokens_with_client` redacts tokens in
/// tracing output when a self-contained OAuth credential refresh fails (HTTP 500).
#[test]
#[cfg(unix)]
fn tracing_output_redacts_tokens_on_self_contained_refresh_failure() {
    let access_token = "SC-ACCESS-SECRET-a1b2c3";
    let refresh_token = "SC-REFRESH-SECRET-d4e5f6";

    // Server returns 500 with tokens echoed in error body.
    let token_url = spawn_json_server(
        500,
        &format!(
            r#"{{"error":"server_error","refresh_token":"{refresh_token}","detail":"internal failure for {access_token}"}}"#,
        ),
    );

    let dir = tempfile::tempdir().expect("tmpdir");
    let auth_path = create_auth_with_expired_oauth(
        dir.path(),
        "test-provider",
        access_token,
        refresh_token,
        Some(token_url),
        Some("self-contained-client".to_string()),
    );

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = CapturingWriter(Arc::clone(&buffer));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(writer)
        .with_ansi(false)
        .finish();

    let result = tracing::subscriber::with_default(subscriber, || {
        common::run_async(async move {
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            let client = pi::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client).await
        })
    });

    // The refresh should fail.
    assert!(result.is_err(), "expected refresh failure");

    let err_text = result.unwrap_err().to_string();
    assert_no_leaked_secrets(&err_text, &[access_token, refresh_token], "error Display");

    let log_output = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert_no_leaked_secrets(
        &log_output,
        &[access_token, refresh_token],
        "tracing output",
    );
}

/// Verify that `Error::Auth` display output does not contain raw tokens when
/// the error message is constructed with `redact_known_secrets_for_tests`.
#[test]
fn error_display_impl_redacts_auth_secrets() {
    let secret_token = "sk-ant-SUPER-SECRET-TOKEN-789";
    let error_body = format!(
        r#"{{"error":"unauthorized","access_token":"{secret_token}","detail":"bad token"}}"#,
    );

    // Simulate what production code does: redact before constructing the error.
    let redacted = redact_known_secrets_for_tests(&error_body, &[secret_token]);
    let error = pi::error::Error::auth(format!("Token refresh failed: {redacted}"));

    let display_text = error.to_string();
    let debug_text = format!("{error:?}");

    assert_no_leaked_secrets(&display_text, &[secret_token], "Error Display");
    assert_no_leaked_secrets(&debug_text, &[secret_token], "Error Debug");
    assert!(
        display_text.contains("[REDACTED]"),
        "expected [REDACTED] marker in error: {display_text}"
    );
}
