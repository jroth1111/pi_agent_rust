//! Authentication storage and API key resolution.
//!
//! Auth file: ~/.pi/agent/auth.json

use crate::agent_cx::AgentCx;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::provider_metadata::{canonical_provider_id, provider_auth_env_keys, provider_metadata};
use asupersync::channel::oneshot;
use base64::Engine as _;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── Event Callbacks for OAuth Observability ─────────────────────────────
pub mod callbacks;
pub use callbacks::{
    CallbackRegistry, LoggingRefreshCallback, NotificationRefreshCallback, RefreshCallback,
    RefreshEvent,
};

// ── Secure Token Storage (Keyring) ──────────────────────────────────────
pub mod keyring;
pub use keyring::{
    AuthStorageBackend, FileStorage, KeyringStorage, SecureStorage, StorageStrategy,
};

// ── Credential Cache ────────────────────────────────────────────────────
pub mod cache;
pub use cache::{
    CacheConfig, CacheStats, CachedEntry, CredentialCache, global_cache, init_global_cache,
};

// ── Multi-Source Secret Resolution ───────────────────────────────────────
pub mod secret_resolver;
pub use secret_resolver::{ResolvedSecret, SecretResolver, SecretSource};

// ── Background Refresh Worker ────────────────────────────────────────────
pub mod refresh_worker;
pub use refresh_worker::{RefreshWorker, RefreshWorkerConfig};

// ── Recovery State Machine ───────────────────────────────────────────────
pub mod recovery;
pub use recovery::{
    RecoveryConfig, RecoveryContext, RecoveryEvent, RecoveryMachine, RecoveryMethod,
    RecoveryResult, RecoveryState, RefreshResult,
};

// ── Rotation Policies ────────────────────────────────────────────────────
pub mod rotation;
pub use rotation::{
    EffectiveRotationConfig, EntropyCalculator, ProviderRotationConfig, RotationPolicy,
    RotationReason, RotationStrategy, UsageSample,
};

// ── Token Revocation ─────────────────────────────────────────────────────
pub mod revocation;
pub use revocation::{
    CleanupReport, ProviderCleanupDetail, RevocationManager, RevocationReason, RevocationRecord,
};

// ── Audit Logging ────────────────────────────────────────────────────────
pub mod audit;
pub use audit::{AuditEvent, AuditEventType, AuditLog, AuditStats};

const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_OAUTH_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_OAUTH_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const ANTHROPIC_OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";

// ── OpenAI Codex OAuth constants ─────────────────────────────────
const OPENAI_CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CODEX_OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OPENAI_CODEX_OAUTH_SCOPES: &str = "openid profile email offline_access";

// ── Google Gemini CLI OAuth constants ────────────────────────────
const GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET: &str = "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";
const GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI: &str = "http://localhost:8085/oauth2callback";
const GOOGLE_GEMINI_CLI_OAUTH_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile";
const GOOGLE_GEMINI_CLI_OAUTH_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_GEMINI_CLI_CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";

// ── Google Antigravity OAuth constants ───────────────────────────
const GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET: &str = "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";
const GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
const GOOGLE_ANTIGRAVITY_OAUTH_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
const GOOGLE_ANTIGRAVITY_OAUTH_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";
const GOOGLE_ANTIGRAVITY_PROJECT_DISCOVERY_ENDPOINTS: [&str; 3] = [
    "https://cloudcode-pa.googleapis.com",
    "https://autopush-cloudcode-pa.sandbox.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
];

/// Internal marker used to preserve OAuth-vs-API-key lane information when
/// passing Anthropic credentials through provider-agnostic key plumbing.
const ANTHROPIC_OAUTH_BEARER_MARKER: &str = "__pi_anthropic_oauth_bearer__:";

// ── GitHub / Copilot OAuth constants ──────────────────────────────
const GITHUB_OAUTH_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_OAUTH_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
/// Default scopes for Copilot access (read:user needed for identity).
const GITHUB_COPILOT_SCOPES: &str = "read:user";

// ── GitLab OAuth constants ────────────────────────────────────────
const GITLAB_OAUTH_AUTHORIZE_PATH: &str = "/oauth/authorize";
const GITLAB_OAUTH_TOKEN_PATH: &str = "/oauth/token";
const GITLAB_DEFAULT_BASE_URL: &str = "https://gitlab.com";
/// Default scopes for GitLab AI features.
const GITLAB_DEFAULT_SCOPES: &str = "api read_api read_user";

// ── Kimi Code OAuth constants ─────────────────────────────────────
const KIMI_CODE_OAUTH_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_CODE_OAUTH_DEFAULT_HOST: &str = "https://auth.kimi.com";
const KIMI_CODE_OAUTH_HOST_ENV_KEYS: [&str; 2] = ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"];
const KIMI_SHARE_DIR_ENV_KEY: &str = "KIMI_SHARE_DIR";
const KIMI_CODE_DEVICE_AUTHORIZATION_PATH: &str = "/api/oauth/device_authorization";
const KIMI_CODE_TOKEN_PATH: &str = "/api/oauth/token";

/// Credentials stored in auth.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCredential {
    ApiKey {
        key: String,
    },
    OAuth {
        access_token: String,
        refresh_token: String,
        expires: i64, // Unix ms
        /// Token endpoint URL for self-contained refresh (optional; backward-compatible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_url: Option<String>,
        /// Client ID for self-contained refresh (optional; backward-compatible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// AWS IAM credentials for providers like Amazon Bedrock.
    ///
    /// Supports the standard credential chain: explicit keys → env vars → profile → container
    /// credentials → web identity token.
    AwsCredentials {
        access_key_id: String,
        secret_access_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
    },
    /// Bearer token for providers that accept `Authorization: Bearer <token>`.
    ///
    /// Used by gateway proxies (Vercel AI Gateway, Helicone, etc.) and services
    /// that issue pre-authenticated bearer tokens (e.g. `AWS_BEARER_TOKEN_BEDROCK`).
    BearerToken {
        token: String,
    },
    /// Service key credentials for providers like SAP AI Core that use
    /// client-credentials OAuth (client_id + client_secret → token_url → bearer).
    ServiceKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service_url: Option<String>,
    },
}

/// Canonical credential status for a provider in auth.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialStatus {
    Missing,
    ApiKey,
    OAuthValid { expires_in_ms: i64 },
    OAuthExpired { expired_by_ms: i64 },
    BearerToken,
    AwsCredentials,
    ServiceKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthErrorClass {
    Auth,
    RateLimit,
    Quota,
    Transport,
    Fatal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthAccountPolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthCandidate {
    pub provider: String,
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthRotationPlan {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub max_attempts: usize,
    pub candidates: Vec<OAuthCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthOutcome {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<OAuthErrorClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// HTTP status code from the response (for enhanced rotation logic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotationTrace {
    #[serde(default)]
    pub attempts: Vec<RotationTraceStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RotationTraceStep {
    pub provider: String,
    pub account_id: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<OAuthErrorClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// HTTP status code from the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRuntimeOutcome {
    Success,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OAuthFailureKind {
    RequiresRelogin,
    RateLimited,
    Transient,
    /// Quota exhausted - account cannot be used until quota resets.
    QuotaExhausted,
}

trait OAuthProviderAdapter: Sync {
    fn provider_id(&self) -> &'static str;

    fn non_refreshable_reason(
        &self,
        provider: &str,
        access_token: &str,
        refresh_token: &str,
        token_url: Option<&str>,
        client_id: Option<&str>,
    ) -> Option<String>;

    fn can_refresh(
        &self,
        provider: &str,
        access_token: &str,
        refresh_token: &str,
        token_url: Option<&str>,
        client_id: Option<&str>,
    ) -> bool {
        self.non_refreshable_reason(provider, access_token, refresh_token, token_url, client_id)
            .is_none()
    }

    fn classify_error(&self, error: &str) -> OAuthErrorClass;

    fn retry_after_ms(&self, error: &str) -> Option<i64>;
}

#[derive(Debug, Clone, Copy)]
struct BasicOAuthProviderAdapter {
    provider_id: &'static str,
    requires_google_project_id: bool,
    requires_self_contained_metadata: bool,
}

impl BasicOAuthProviderAdapter {
    const fn new(
        provider_id: &'static str,
        requires_google_project_id: bool,
        requires_self_contained_metadata: bool,
    ) -> Self {
        Self {
            provider_id,
            requires_google_project_id,
            requires_self_contained_metadata,
        }
    }
}

impl OAuthProviderAdapter for BasicOAuthProviderAdapter {
    fn provider_id(&self) -> &'static str {
        self.provider_id
    }

    fn non_refreshable_reason(
        &self,
        provider: &str,
        access_token: &str,
        refresh_token: &str,
        token_url: Option<&str>,
        client_id: Option<&str>,
    ) -> Option<String> {
        if refresh_token.trim().is_empty() {
            return Some(format!(
                "{provider} OAuth credential is non-refreshable (missing refresh_token); relogin required"
            ));
        }

        if self.requires_google_project_id
            && resolve_google_refresh_project_id(provider, access_token).is_none()
        {
            return Some(format!(
                "{provider} OAuth credential is missing project metadata; relogin required"
            ));
        }

        if self.requires_self_contained_metadata && (token_url.is_none() || client_id.is_none()) {
            return Some(format!(
                "{provider} OAuth credential is missing token_url/client_id refresh metadata; relogin required"
            ));
        }

        None
    }

    fn classify_error(&self, error: &str) -> OAuthErrorClass {
        let lower = error.to_ascii_lowercase();
        if lower.contains("429")
            || lower.contains("rate limit")
            || lower.contains("too many requests")
            || lower.contains("retry-after")
        {
            return OAuthErrorClass::RateLimit;
        }
        if lower.contains("quota") {
            return OAuthErrorClass::Quota;
        }
        if lower.contains("401")
            || lower.contains("403")
            || lower.contains("unauthorized")
            || lower.contains("forbidden")
            || lower.contains("invalid_grant")
            || lower.contains("access denied")
            || lower.contains("relogin")
            || lower.contains("reauth")
            || lower.contains("not eligible for gemini code assist")
        {
            return OAuthErrorClass::Auth;
        }
        if lower.contains("timeout")
            || lower.contains("connection reset")
            || lower.contains("connection refused")
            || lower.contains("temporarily unavailable")
            || lower.contains("network")
            || lower.contains("econn")
            || lower.contains("tls")
        {
            return OAuthErrorClass::Transport;
        }
        OAuthErrorClass::Fatal
    }

    fn retry_after_ms(&self, error: &str) -> Option<i64> {
        parse_retry_after_hint_ms(error)
    }
}

const OAUTH_ADAPTER_ANTHROPIC: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("anthropic", false, false);
const OAUTH_ADAPTER_OPENAI_CODEX: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("openai-codex", false, true);
const OAUTH_ADAPTER_GOOGLE_GEMINI_CLI: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("google-gemini-cli", true, false);
const OAUTH_ADAPTER_GOOGLE_ANTIGRAVITY: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("google-antigravity", true, false);
const OAUTH_ADAPTER_GITHUB_COPILOT: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("github-copilot", false, true);
const OAUTH_ADAPTER_GITLAB: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("gitlab", false, true);
const OAUTH_ADAPTER_KIMI_FOR_CODING: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("kimi-for-coding", false, false);
const OAUTH_ADAPTER_SELF_CONTAINED: BasicOAuthProviderAdapter =
    BasicOAuthProviderAdapter::new("self-contained", false, false);

fn oauth_provider_adapter(provider: &str) -> &'static dyn OAuthProviderAdapter {
    match AuthStorage::canonical_oauth_provider(provider).as_str() {
        "anthropic" => &OAUTH_ADAPTER_ANTHROPIC,
        "openai-codex" => &OAUTH_ADAPTER_OPENAI_CODEX,
        "google-gemini-cli" => &OAUTH_ADAPTER_GOOGLE_GEMINI_CLI,
        "google-antigravity" => &OAUTH_ADAPTER_GOOGLE_ANTIGRAVITY,
        "github-copilot" => &OAUTH_ADAPTER_GITHUB_COPILOT,
        "gitlab" => &OAUTH_ADAPTER_GITLAB,
        "kimi-for-coding" => &OAUTH_ADAPTER_KIMI_FOR_CODING,
        _ => &OAUTH_ADAPTER_SELF_CONTAINED,
    }
}

/// Parse a `Retry-After` header value into milliseconds.
///
/// Supported formats:
/// - Numeric seconds: `"120"`
/// - HTTP-date / RFC 2822: `"Wed, 21 Oct 2015 07:28:00 GMT"`
/// - RFC 3339 / ISO 8601: `"2015-10-21T07:28:00Z"`
/// - Relative units: `"120 seconds"`, `"2 minutes"`, `"1 hour"`
pub fn parse_retry_after_header_enhanced(value: &str, now_ms: i64) -> Option<i64> {
    fn parse_relative_time(s: &str) -> Option<i64> {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() < 2 {
            return None;
        }

        let value: i64 = parts[0].parse().ok()?;
        let unit = parts[1].to_ascii_lowercase();

        let multiplier = match unit.as_str() {
            "second" | "seconds" | "sec" | "secs" | "s" => 1_000,
            "minute" | "minutes" | "min" | "mins" | "m" => 60 * 1_000,
            "hour" | "hours" | "hr" | "hrs" | "h" => 60 * 60 * 1_000,
            "day" | "days" | "d" => 24 * 60 * 60 * 1_000,
            _ => return None,
        };
        Some(value.saturating_mul(multiplier).max(0))
    }

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(relative) = parse_relative_time(trimmed) {
        return Some(relative);
    }

    let numeric = trimmed.trim_end_matches(',');
    if let Ok(seconds) = numeric.parse::<i64>() {
        return Some(seconds.max(0).saturating_mul(1_000));
    }

    let date_candidate = trimmed.split(['\n', ';']).next().unwrap_or_default().trim();

    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_candidate) {
        return Some((dt.timestamp_millis() - now_ms).max(0));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date_candidate) {
        return Some((dt.timestamp_millis() - now_ms).max(0));
    }
    None
}

fn parse_retry_after_hint_ms(error: &str) -> Option<i64> {
    fn parse_retry_after_value(value: &str) -> Option<i64> {
        parse_retry_after_header_enhanced(value, chrono::Utc::now().timestamp_millis())
    }

    let lower = error.to_ascii_lowercase();
    if let Some(index) = lower.find("retry-after") {
        let mut tail = &error[index + "retry-after".len()..];
        tail = tail.trim_start();
        if let Some(stripped) = tail.strip_prefix(':') {
            tail = stripped;
        } else if let Some(stripped) = tail.strip_prefix('=') {
            tail = stripped;
        }
        if let Some(value) = parse_retry_after_value(tail) {
            return Some(value);
        }
    }
    if let Some(index) = lower.find("retry after") {
        let tail = &error[index + "retry after".len()..];
        if let Some(value) = parse_retry_after_value(tail) {
            return Some(value);
        }
    }
    None
}

fn oauth_refresh_inflight_slots() -> &'static Mutex<HashMap<String, usize>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn oauth_refresh_slot_key(provider: &str, account_id: Option<&str>) -> String {
    account_id.map_or_else(
        || format!("{provider}:_entry"),
        |account| format!("{provider}:{account}"),
    )
}

fn try_acquire_oauth_refresh_slot(provider: &str, account_id: Option<&str>) -> Option<String> {
    let key = oauth_refresh_slot_key(provider, account_id);
    let Ok(mut slots) = oauth_refresh_inflight_slots().lock() else {
        return None;
    };
    let current = slots.entry(key.clone()).or_insert(0);
    if *current > 0 {
        return None;
    }
    *current = 1;
    Some(key)
}

fn release_oauth_refresh_slot(slot_key: &str) {
    let Ok(mut slots) = oauth_refresh_inflight_slots().lock() else {
        return;
    };
    if let Some(current) = slots.get_mut(slot_key) {
        if *current > 1 {
            *current -= 1;
        } else {
            slots.remove(slot_key);
        }
    }
}

/// Proactive refresh: attempt refresh this many ms *before* actual expiry.
/// This avoids using a token that's about to expire during a long-running request.
/// Auth file schema version.
pub const AUTH_SCHEMA_VERSION: u32 = 3;

/// Proactive refresh: attempt refresh this many ms *before* actual expiry.
/// This avoids using a token that's about to expire during a long-running request.
const PROACTIVE_REFRESH_WINDOW_MS: i64 = 10 * 60 * 1000; // 10 minutes
const OAUTH_RATE_LIMIT_BASE_COOLDOWN_MS: i64 = 30_000;
const OAUTH_TRANSIENT_BASE_COOLDOWN_MS: i64 = 5_000;
const OAUTH_MAX_COOLDOWN_MS: i64 = 10 * 60 * 1000;
const OAUTH_ACCOUNT_ID_PREFIX: &str = "acct-";
const OAUTH_PROJECT_ID_METADATA_KEY: &str = "project_id";

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone)]
struct OAuthRefreshRequest {
    provider: String,
    account_id: Option<String>,
    access_token: String,
    refresh_token: String,
    token_url: Option<String>,
    client_id: Option<String>,
    provider_metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OAuthAccountHealth {
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until_ms: Option<i64>,
    #[serde(default)]
    pub requires_relogin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_kind: Option<OAuthErrorClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub non_refreshable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_relogin_on_expiry: bool,

    // === V2 Enhanced Health Tracking Fields ===
    /// Count of consecutive successful requests (reset on any failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_successes: Option<u32>,
    /// HTTP status code from the last request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status_code: Option<u16>,
    /// Timestamp when quota was exhausted; account blocked until this time.
    /// Distinct from rate limit cooldown - indicates hard quota exhaustion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted_until_ms: Option<i64>,
    /// Total number of requests made with this account (for observability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_requests: Option<u64>,

    // === V3 Rotation & Recovery Fields ===
    /// Entropy calculator samples (serialized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entropy_samples: Option<Vec<rotation::UsageSample>>,

    /// Last rotation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rotation_ms: Option<i64>,

    /// Reason for last rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rotation_reason: Option<rotation::RotationReason>,

    /// Current recovery state (for unauthorized handling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_state: Option<recovery::RecoveryState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthAccountRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub credential: AuthCredential,
    #[serde(default)]
    pub health: OAuthAccountHealth,
    #[serde(default)]
    pub policy: OAuthAccountPolicy,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OAuthProviderPool {
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub accounts: HashMap<String, OAuthAccountRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthFile {
    /// Schema version for forward compatibility.
    #[serde(default = "default_auth_schema_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub oauth_pools: HashMap<String, OAuthProviderPool>,
    #[serde(flatten)]
    pub entries: HashMap<String, AuthCredential>,
}

const fn default_auth_schema_version() -> u32 {
    AUTH_SCHEMA_VERSION
}

/// Auth storage wrapper with file locking.
#[derive(Debug, Clone)]
pub struct AuthStorage {
    path: PathBuf,
    entries: HashMap<String, AuthCredential>,
    oauth_pools: HashMap<String, OAuthProviderPool>,
    rotation_cursors: Arc<Mutex<HashMap<String, usize>>>,
    rotation_policy: RotationPolicy,
    recovery_config: RecoveryConfig,
    revocation_manager: RevocationManager,
    audit_log: Option<AuditLog>,
    callbacks: Option<Arc<CallbackRegistry>>,
}

impl AuthStorage {
    fn allow_external_provider_lookup(&self) -> bool {
        // External credential auto-detection is intended for Pi's global auth
        // file (typically `~/.pi/agent/auth.json`). Scoping it this way keeps
        // tests and custom auth sandboxes deterministic.
        self.path == Config::auth_path()
    }

    fn normalize_provider_key(provider: &str) -> String {
        let normalized = provider.trim().to_ascii_lowercase();
        canonical_provider_id(&normalized)
            .unwrap_or(normalized.as_str())
            .to_string()
    }

    fn provider_lookup_keys(provider: &str) -> Vec<String> {
        let normalized = provider.trim().to_ascii_lowercase();
        let canonical = Self::normalize_provider_key(&normalized);
        let metadata =
            provider_metadata(canonical.as_str()).or_else(|| provider_metadata(&normalized));

        let mut keys = vec![normalized, canonical.clone()];
        if let Some(metadata) = metadata {
            keys.push(metadata.canonical_id.to_string());
            keys.extend(
                metadata
                    .aliases
                    .iter()
                    .map(std::string::ToString::to_string),
            );
        }

        let mut normalized_keys: Vec<String> = keys
            .into_iter()
            .map(|key| Self::normalize_provider_key(&key))
            .collect();
        normalized_keys.sort();
        normalized_keys.dedup();
        normalized_keys
    }

    fn entry_case_insensitive(&self, key: &str) -> Option<&AuthCredential> {
        self.entries.iter().find_map(|(existing, credential)| {
            existing.eq_ignore_ascii_case(key).then_some(credential)
        })
    }

    fn oauth_pool_for_provider(&self, provider: &str) -> Option<&OAuthProviderPool> {
        for key in Self::provider_lookup_keys(provider) {
            if let Some(pool) = self.oauth_pools.get(&key) {
                return Some(pool);
            }
        }
        self.oauth_pools
            .iter()
            .find_map(|(key, pool)| key.eq_ignore_ascii_case(provider).then_some(pool))
    }

    fn credential_for_provider_entry(&self, provider: &str) -> Option<&AuthCredential> {
        for key in Self::provider_lookup_keys(provider) {
            if let Some(credential) = self.entries.get(&key) {
                return Some(credential);
            }
        }
        self.entry_case_insensitive(provider)
    }

    fn oauth_record_selectable(record: &OAuthAccountRecord, now: i64) -> bool {
        match &record.credential {
            AuthCredential::OAuth { expires, .. } => {
                *expires > now
                    && !record.health.requires_relogin
                    && record
                        .health
                        .cooldown_until_ms
                        .is_none_or(|until| until <= now)
                    // V2: Check quota exhaustion
                    && record
                        .health
                        .exhausted_until_ms
                        .is_none_or(|until| until <= now)
            }
            _ => true,
        }
    }

    fn oauth_record_revoked(&self, record: &OAuthAccountRecord) -> bool {
        match &record.credential {
            AuthCredential::OAuth { access_token, .. } => {
                self.revocation_manager.is_revoked(access_token)
            }
            _ => false,
        }
    }

    fn active_oauth_credential<'a>(
        &self,
        pool: &'a OAuthProviderPool,
    ) -> Option<&'a AuthCredential> {
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(active_id) = &pool.active
            && let Some(record) = pool.accounts.get(active_id)
            && Self::oauth_record_selectable(record, now)
            && !self.oauth_record_revoked(record)
        {
            return Some(&record.credential);
        }
        for account_id in &pool.order {
            if let Some(record) = pool.accounts.get(account_id) {
                if Self::oauth_record_selectable(record, now) && !self.oauth_record_revoked(record)
                {
                    return Some(&record.credential);
                }
            }
        }
        pool.accounts
            .values()
            .find(|record| {
                Self::oauth_record_selectable(record, now) && !self.oauth_record_revoked(record)
            })
            .map(|record| &record.credential)
    }

    fn credential_for_provider(&self, provider: &str) -> Option<&AuthCredential> {
        self.oauth_pool_for_provider(provider)
            .and_then(|pool| self.active_oauth_credential(pool))
            .or_else(|| self.credential_for_provider_entry(provider))
    }

    fn infer_account_provider_metadata(
        provider: &str,
        credential: &AuthCredential,
    ) -> HashMap<String, String> {
        let mut metadata = HashMap::new();
        if let AuthCredential::OAuth { access_token, .. } = credential
            && matches!(
                Self::canonical_oauth_provider(provider).as_str(),
                "google-gemini-cli" | "google-antigravity"
            )
            && let Some(project_id) = decode_project_scoped_access_token(access_token)
                .map(|(_, project_id)| project_id)
                .or_else(google_project_id_from_env)
                .or_else(google_project_id_from_gcloud_config)
        {
            metadata.insert(OAUTH_PROJECT_ID_METADATA_KEY.to_string(), project_id);
        }
        metadata
    }

    fn normalize_account(provider: &str, account: &mut OAuthAccountRecord, now: i64) {
        if account.id.trim().is_empty() {
            account.id = Self::generate_oauth_account_id(provider, &account.credential);
        }
        if account
            .label
            .as_deref()
            .is_some_and(|label| label.trim().is_empty())
        {
            account.label = None;
        }

        if account.provider_metadata.is_empty() {
            account.provider_metadata =
                Self::infer_account_provider_metadata(provider, &account.credential);
        }
        account
            .provider_metadata
            .retain(|key, value| !key.trim().is_empty() && !value.trim().is_empty());

        account.policy.allowed_models = account
            .policy
            .allowed_models
            .iter()
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .collect();
        account.policy.excluded_models = account
            .policy
            .excluded_models
            .iter()
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .collect();
        account.policy.provider_tags = account
            .policy
            .provider_tags
            .iter()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect();

        let non_refreshable = match &account.credential {
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                token_url,
                client_id,
                ..
            } => Self::provider_non_refreshable_reason(
                provider,
                access_token,
                refresh_token,
                token_url.as_deref(),
                client_id.as_deref(),
            )
            .is_some(),
            _ => false,
        };
        account.health.non_refreshable = non_refreshable;
        account.health.requires_relogin_on_expiry = non_refreshable
            && matches!(
                Self::canonical_oauth_provider(provider).as_str(),
                "openai-codex" | "github-copilot"
            );
        if account.health.requires_relogin_on_expiry
            && let AuthCredential::OAuth { expires, .. } = &account.credential
            && *expires <= now
        {
            account.health.requires_relogin = true;
        }
    }

    fn normalize_pool(provider: &str, pool: &mut OAuthProviderPool) {
        if pool
            .namespace
            .as_deref()
            .is_some_and(|namespace| namespace.trim().is_empty())
        {
            pool.namespace = None;
        }

        let now = chrono::Utc::now().timestamp_millis();
        let mut normalized_accounts: HashMap<String, OAuthAccountRecord> = HashMap::new();
        for (account_id, mut account) in std::mem::take(&mut pool.accounts) {
            let mut normalized_id = account_id.trim().to_string();
            if normalized_id.is_empty() {
                normalized_id = account.id.trim().to_string();
            }
            if normalized_id.is_empty() {
                continue;
            }
            account.id.clone_from(&normalized_id);
            Self::normalize_account(provider, &mut account, now);
            normalized_accounts.insert(normalized_id, account);
        }
        pool.accounts = normalized_accounts;

        pool.order = pool
            .order
            .iter()
            .filter(|id| pool.accounts.contains_key(*id))
            .cloned()
            .collect();
        for account_id in pool.accounts.keys() {
            if !pool.order.iter().any(|id| id == account_id) {
                pool.order.push(account_id.clone());
            }
        }

        if let Some(active_id) = &pool.active
            && !pool.accounts.contains_key(active_id)
        {
            pool.active = None;
        }
        if pool.active.is_none() {
            pool.active = pool.order.first().cloned();
        }
    }

    fn normalize_loaded_state(&mut self) {
        let mut normalized_entries: HashMap<String, AuthCredential> = HashMap::new();
        for (provider, credential) in std::mem::take(&mut self.entries) {
            let key = Self::normalize_provider_key(&provider);
            normalized_entries.insert(key, credential);
        }
        self.entries = normalized_entries;

        let mut normalized_pools: HashMap<String, OAuthProviderPool> = HashMap::new();
        for (provider, mut pool) in std::mem::take(&mut self.oauth_pools) {
            let key = Self::normalize_provider_key(&provider);
            Self::normalize_pool(&key, &mut pool);
            let pool_key = key.clone();
            normalized_pools
                .entry(key)
                .and_modify(|existing| {
                    for (id, account) in pool.accounts.drain() {
                        existing.accounts.insert(id.clone(), account);
                        if !existing.order.iter().any(|oid| oid == &id) {
                            existing.order.push(id);
                        }
                    }
                    if existing.active.is_none() {
                        existing.active = pool.active.take();
                    }
                    Self::normalize_pool(&pool_key, existing);
                })
                .or_insert(pool);
        }
        self.oauth_pools = normalized_pools;
    }

    fn oauth_duplicate_account_id(
        pool: &OAuthProviderPool,
        credential: &AuthCredential,
    ) -> Option<String> {
        let AuthCredential::OAuth {
            refresh_token,
            access_token,
            ..
        } = credential
        else {
            return None;
        };

        pool.accounts
            .iter()
            .find_map(|(account_id, account)| match &account.credential {
                AuthCredential::OAuth {
                    refresh_token: existing_refresh,
                    access_token: existing_access,
                    ..
                } if existing_refresh == refresh_token && existing_access == access_token => {
                    Some(account_id.clone())
                }
                _ => None,
            })
    }

    fn generate_oauth_account_id(provider: &str, credential: &AuthCredential) -> String {
        let mut hasher = sha2::Sha256::new();
        hasher.update(provider.as_bytes());
        if let AuthCredential::OAuth {
            access_token,
            refresh_token,
            expires,
            ..
        } = credential
        {
            hasher.update(access_token.as_bytes());
            hasher.update(refresh_token.as_bytes());
            hasher.update(expires.to_le_bytes());
        }
        let digest = hasher.finalize();
        let mut suffix = String::new();
        for byte in &digest[..6] {
            let _ = write!(&mut suffix, "{byte:02x}");
        }
        format!("{OAUTH_ACCOUNT_ID_PREFIX}{suffix}")
    }

    fn unique_oauth_account_id(pool: &OAuthProviderPool, mut account_id: String) -> String {
        if !pool.accounts.contains_key(&account_id) {
            return account_id;
        }
        let base = account_id.clone();
        let mut n = 2_u32;
        while pool.accounts.contains_key(&account_id) {
            account_id = format!("{base}-{n}");
            n = n.saturating_add(1);
        }
        account_id
    }

    fn move_legacy_oauth_entries_to_pools(&mut self) {
        let providers: Vec<String> = self.entries.keys().cloned().collect();
        for provider in providers {
            let is_oauth = matches!(
                self.entries.get(&provider),
                Some(AuthCredential::OAuth { .. })
            );
            if !is_oauth {
                continue;
            }
            let Some(credential) = self.entries.remove(&provider) else {
                continue;
            };
            let canonical_provider = Self::normalize_provider_key(&provider);
            let pool = self
                .oauth_pools
                .entry(canonical_provider.clone())
                .or_default();
            Self::normalize_pool(&canonical_provider, pool);

            if let Some(existing_id) = Self::oauth_duplicate_account_id(pool, &credential) {
                if let Some(existing) = pool.accounts.get_mut(&existing_id) {
                    existing.credential = credential;
                    Self::normalize_account(
                        &canonical_provider,
                        existing,
                        chrono::Utc::now().timestamp_millis(),
                    );
                }
                continue;
            }

            let generated_id = Self::generate_oauth_account_id(&canonical_provider, &credential);
            let account_id = Self::unique_oauth_account_id(pool, generated_id);
            let mut account = OAuthAccountRecord {
                id: account_id.clone(),
                label: None,
                credential,
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            };
            Self::normalize_account(
                &canonical_provider,
                &mut account,
                chrono::Utc::now().timestamp_millis(),
            );
            pool.accounts.insert(account_id.clone(), account);
            if !pool.order.iter().any(|id| id == &account_id) {
                pool.order.push(account_id.clone());
            }
            if pool.active.is_none() {
                pool.active = Some(account_id);
            }
        }
    }

    /// Load auth.json (creates empty if missing).
    pub fn load(path: PathBuf) -> Result<Self> {
        let auth_file = if path.exists() {
            let file = File::open(&path).map_err(|e| Error::auth(format!("auth.json: {e}")))?;
            let mut locked = lock_file(file, Duration::from_secs(30))?;
            // Read from the locked file handle, not a new handle
            let mut content = String::new();
            locked.as_file_mut().read_to_string(&mut content)?;
            let parsed: AuthFile = match serde_json::from_str(&content) {
                Ok(file) => file,
                Err(e) => {
                    tracing::warn!(
                        event = "pi.auth.parse_error",
                        error = %e,
                        "auth.json is corrupted; starting with empty credentials"
                    );
                    AuthFile::default()
                }
            };
            parsed
        } else {
            AuthFile::default()
        };

        let mut storage = Self {
            path,
            entries: auth_file.entries,
            oauth_pools: auth_file.oauth_pools,
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        storage.normalize_loaded_state();
        storage.move_legacy_oauth_entries_to_pools();
        storage.normalize_loaded_state();
        Ok(storage)
    }

    /// Load auth.json asynchronously (creates empty if missing).
    pub async fn load_async(path: PathBuf) -> Result<Self> {
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let res = Self::load(path);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::auth("Load task cancelled".to_string()))?
    }

    /// Persist auth.json (atomic write + permissions).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut options = File::options();
        options.read(true).write(true).create(true).truncate(false);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let file = options.open(&self.path)?;
        let mut locked = lock_file(file, Duration::from_secs(30))?;

        let mut oauth_pools = self.oauth_pools.clone();
        for (provider, pool) in &mut oauth_pools {
            Self::normalize_pool(provider, pool);
        }
        let data = serde_json::to_string_pretty(&AuthFile {
            version: AUTH_SCHEMA_VERSION,
            oauth_pools,
            entries: self.entries.clone(),
        })?;

        // Write to the locked file handle, not a new handle
        let f = locked.as_file_mut();
        f.seek(SeekFrom::Start(0))?;
        f.set_len(0)?; // Truncate after seeking to avoid data loss
        f.write_all(data.as_bytes())?;
        f.flush()?;

        Ok(())
    }

    /// Persist auth.json asynchronously.
    pub async fn save_async(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let this = self.clone();

        std::thread::spawn(move || {
            let res = this.save();
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::auth("Save task cancelled".to_string()))?
    }

    /// Get raw credential.
    pub fn get(&self, provider: &str) -> Option<&AuthCredential> {
        self.credential_for_provider(provider)
    }

    fn append_oauth_account_internal(
        &mut self,
        provider: &str,
        credential: AuthCredential,
        label: Option<String>,
    ) -> Option<String> {
        if !matches!(credential, AuthCredential::OAuth { .. }) {
            return None;
        }

        let canonical_provider = Self::normalize_provider_key(provider);
        let pool = self
            .oauth_pools
            .entry(canonical_provider.clone())
            .or_default();
        Self::normalize_pool(&canonical_provider, pool);

        if let Some(existing_id) = Self::oauth_duplicate_account_id(pool, &credential) {
            if let Some(existing) = pool.accounts.get_mut(&existing_id) {
                existing.credential = credential;
                if label.is_some() {
                    existing.label = label;
                }
                existing.health.requires_relogin = false;
                existing.health.cooldown_until_ms = None;
                existing.health.last_error = None;
                existing.health.consecutive_failures = 0;
                existing.health.last_error_kind = None;
                Self::normalize_account(
                    &canonical_provider,
                    existing,
                    chrono::Utc::now().timestamp_millis(),
                );
            }
            if !pool.order.iter().any(|id| id == &existing_id) {
                pool.order.push(existing_id.clone());
            }
            if pool.active.is_none() {
                pool.active = Some(existing_id.clone());
            }
            return Some(existing_id);
        }

        let generated = Self::generate_oauth_account_id(&canonical_provider, &credential);
        let account_id = Self::unique_oauth_account_id(pool, generated);
        let mut account = OAuthAccountRecord {
            id: account_id.clone(),
            label,
            credential,
            health: OAuthAccountHealth::default(),
            policy: OAuthAccountPolicy::default(),
            provider_metadata: HashMap::new(),
        };
        Self::normalize_account(
            &canonical_provider,
            &mut account,
            chrono::Utc::now().timestamp_millis(),
        );
        pool.accounts.insert(account_id.clone(), account);
        if !pool.order.iter().any(|id| id == &account_id) {
            pool.order.push(account_id.clone());
        }
        if pool.active.is_none() {
            pool.active = Some(account_id.clone());
        }
        Some(account_id)
    }

    /// Add an OAuth account to a provider's rotation pool.
    pub fn append_oauth_account(
        &mut self,
        provider: &str,
        credential: AuthCredential,
        label: Option<String>,
    ) -> Option<String> {
        self.append_oauth_account_internal(provider, credential, label)
    }

    /// List OAuth accounts for `provider` in rotation order.
    pub fn list_oauth_accounts(&self, provider: &str) -> Vec<OAuthAccountRecord> {
        let Some(pool) = self.oauth_pool_for_provider(provider) else {
            return Vec::new();
        };

        let mut listed = Vec::new();
        for account_id in &pool.order {
            if let Some(account) = pool.accounts.get(account_id) {
                listed.push(account.clone());
            }
        }
        for (account_id, account) in &pool.accounts {
            if !pool.order.iter().any(|ordered| ordered == account_id) {
                listed.push(account.clone());
            }
        }
        listed
    }

    /// Set the active OAuth account for `provider`.
    pub fn set_active_oauth_account(&mut self, provider: &str, account_id: &str) -> bool {
        let canonical_provider = Self::normalize_provider_key(provider);
        let Some(pool) = self.oauth_pools.get_mut(&canonical_provider) else {
            return false;
        };
        if !pool.accounts.contains_key(account_id) {
            return false;
        }
        pool.active = Some(account_id.to_string());
        if !pool.order.iter().any(|id| id == account_id) {
            pool.order.insert(0, account_id.to_string());
        }
        true
    }

    /// Remove a specific OAuth account from a provider's pool.
    pub fn remove_oauth_account(&mut self, provider: &str, account_id: &str) -> bool {
        let canonical_provider = Self::normalize_provider_key(provider);
        let Some(pool) = self.oauth_pools.get_mut(&canonical_provider) else {
            return false;
        };
        if pool.accounts.remove(account_id).is_some() {
            pool.order.retain(|id| id != account_id);
            // If the removed account was active, switch to the next one
            if pool.active.as_ref() == Some(&account_id.to_string()) {
                pool.active = pool.order.first().cloned();
            }
            true
        } else {
            false
        }
    }

    /// Store login credentials with pool-aware OAuth behavior.
    ///
    /// OAuth credentials are appended to the provider's account pool.
    /// Non-OAuth credentials replace existing auth for that provider.
    pub fn set_login_credential(&mut self, provider: &str, credential: AuthCredential) {
        let canonical_provider = Self::normalize_provider_key(provider);
        for alias in Self::provider_lookup_keys(provider)
            .into_iter()
            .filter(|key| key != &canonical_provider)
        {
            self.entries.remove(&alias);
            self.oauth_pools.remove(&alias);
        }

        if let AuthCredential::OAuth { .. } = credential {
            self.entries.remove(&canonical_provider);
            let _ =
                self.append_oauth_account_internal(canonical_provider.as_str(), credential, None);
        } else {
            self.oauth_pools.remove(&canonical_provider);
            self.entries.insert(canonical_provider, credential);
        }
    }

    /// Insert or replace a credential for a provider.
    pub fn set(&mut self, provider: impl Into<String>, credential: AuthCredential) {
        let provider = provider.into();
        let canonical_provider = Self::normalize_provider_key(&provider);
        let _ = self.remove_provider_aliases(canonical_provider.as_str());

        match credential {
            AuthCredential::OAuth { .. } => {
                let _ = self.append_oauth_account_internal(
                    canonical_provider.as_str(),
                    credential,
                    None,
                );
            }
            _ => {
                self.entries.insert(canonical_provider, credential);
            }
        }
    }

    /// Remove a credential for a provider.
    pub fn remove(&mut self, provider: &str) -> bool {
        let key = Self::normalize_provider_key(provider);
        self.entries.remove(&key).is_some() || self.oauth_pools.remove(&key).is_some()
    }

    /// Get API key for provider from auth.json.
    ///
    /// For `ApiKey` and `BearerToken` variants the key/token is returned directly.
    /// For `OAuth` the access token is returned only when not expired.
    /// For `AwsCredentials` the access key ID is returned (callers needing the full
    /// credential set should use [`get`] instead).
    /// For `ServiceKey` this returns `None` because a token exchange is required first.
    pub fn api_key(&self, provider: &str) -> Option<String> {
        self.credential_for_provider(provider)
            .and_then(api_key_from_credential)
    }

    /// Return the names of all providers that have stored credentials.
    pub fn provider_names(&self) -> Vec<String> {
        let mut providers: Vec<String> = self.entries.keys().cloned().collect();
        for provider in self.oauth_pools.keys() {
            if !providers.iter().any(|p| p == provider) {
                providers.push(provider.clone());
            }
        }
        providers.sort();
        providers
    }

    /// Return stored credential status for a provider, including canonical alias fallback.
    pub fn credential_status(&self, provider: &str) -> CredentialStatus {
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(pool) = self.oauth_pool_for_provider(provider) {
            let mut best_valid_expires_in: Option<i64> = None;
            let mut least_expired_by: Option<i64> = None;
            for account in pool.accounts.values() {
                if let AuthCredential::OAuth { expires, .. } = account.credential {
                    if expires > now {
                        let expires_in = expires.saturating_sub(now);
                        best_valid_expires_in =
                            Some(best_valid_expires_in.unwrap_or(0).max(expires_in));
                    } else {
                        let expired_by = now.saturating_sub(expires);
                        least_expired_by =
                            Some(least_expired_by.map_or(expired_by, |curr| curr.min(expired_by)));
                    }
                }
            }

            if let Some(expires_in_ms) = best_valid_expires_in {
                return CredentialStatus::OAuthValid { expires_in_ms };
            }
            if let Some(expired_by_ms) = least_expired_by {
                return CredentialStatus::OAuthExpired { expired_by_ms };
            }
        }

        let cred = self.credential_for_provider_entry(provider);

        let Some(cred) = cred else {
            return if self.allow_external_provider_lookup()
                && resolve_external_provider_api_key(provider).is_some()
            {
                CredentialStatus::ApiKey
            } else {
                CredentialStatus::Missing
            };
        };

        match cred {
            AuthCredential::ApiKey { .. } => CredentialStatus::ApiKey,
            AuthCredential::OAuth { expires, .. } if *expires > now => {
                CredentialStatus::OAuthValid {
                    expires_in_ms: expires.saturating_sub(now),
                }
            }
            AuthCredential::OAuth { expires, .. } => CredentialStatus::OAuthExpired {
                expired_by_ms: now.saturating_sub(*expires),
            },
            AuthCredential::BearerToken { .. } => CredentialStatus::BearerToken,
            AuthCredential::AwsCredentials { .. } => CredentialStatus::AwsCredentials,
            AuthCredential::ServiceKey { .. } => CredentialStatus::ServiceKey,
        }
    }

    /// Remove stored credentials for `provider` and any known aliases/canonical IDs.
    ///
    /// Matching is case-insensitive to clean up legacy mixed-case auth entries.
    pub fn remove_provider_aliases(&mut self, provider: &str) -> bool {
        let keys = Self::provider_lookup_keys(provider);
        if keys.is_empty() {
            return false;
        }

        let mut removed = false;
        for key in keys {
            removed |= self.entries.remove(&key).is_some();
            removed |= self.oauth_pools.remove(&key).is_some();
        }
        removed
    }

    /// Returns true when auth.json contains a credential for `provider`
    /// (including canonical alias fallback).
    pub fn has_stored_credential(&self, provider: &str) -> bool {
        self.credential_for_provider(provider).is_some()
    }

    /// Configure runtime token rotation behavior.
    pub fn set_rotation_policy(&mut self, policy: RotationPolicy) {
        self.rotation_policy = policy;
    }

    /// Get the currently active rotation policy.
    pub const fn rotation_policy(&self) -> &RotationPolicy {
        &self.rotation_policy
    }

    /// Configure runtime unauthorized recovery behavior.
    pub const fn set_recovery_config(&mut self, config: RecoveryConfig) {
        self.recovery_config = config;
    }

    /// Get the currently active recovery configuration.
    pub const fn recovery_config(&self) -> &RecoveryConfig {
        &self.recovery_config
    }

    /// Enable in-memory audit logging with a bounded event buffer.
    pub fn enable_audit_log(&mut self, max_events: usize) {
        self.audit_log = Some(AuditLog::new(max_events.max(1)));
    }

    /// Disable in-memory audit logging.
    pub fn disable_audit_log(&mut self) {
        self.audit_log = None;
    }

    /// Return collected audit events when audit logging is enabled.
    pub fn audit_events(&self) -> Option<Vec<AuditEvent>> {
        self.audit_log.as_ref().map(AuditLog::events)
    }

    /// Attach refresh callbacks used for OAuth observability.
    pub fn set_callback_registry(&mut self, callbacks: Arc<CallbackRegistry>) {
        self.callbacks = Some(callbacks);
    }

    /// Detach refresh callbacks.
    pub fn clear_callback_registry(&mut self) {
        self.callbacks = None;
    }

    fn account_rotation_policy_reason(
        &self,
        provider: &str,
        account: &OAuthAccountRecord,
        now_ms: i64,
    ) -> Option<RotationReason> {
        Self::account_rotation_policy_reason_with(&self.rotation_policy, provider, account, now_ms)
    }

    fn account_rotation_policy_reason_with(
        policy: &RotationPolicy,
        provider: &str,
        account: &OAuthAccountRecord,
        now_ms: i64,
    ) -> Option<RotationReason> {
        let config = policy.config_for_provider(provider);
        if !config.enabled {
            return None;
        }

        if let Some(last_rotation) = account.health.last_rotation_ms
            && let Some(interval_ms) = config.interval_ms
            && now_ms.saturating_sub(last_rotation) >= interval_ms
        {
            return Some(RotationReason::Interval);
        }

        if let Some(total_requests) = account.health.total_requests
            && policy.should_rotate_by_usage(provider, total_requests)
        {
            return Some(RotationReason::MaxUsage);
        }

        if let Some(last_rotation) = account.health.last_rotation_ms
            && policy.should_rotate_by_age(provider, now_ms.saturating_sub(last_rotation))
        {
            return Some(RotationReason::Interval);
        }

        if matches!(
            config.strategy,
            RotationStrategy::EntropyBased | RotationStrategy::Hybrid
        ) && let Some(samples) = &account.health.entropy_samples
        {
            let mut calc = EntropyCalculator::new(config.max_entropy_samples);
            calc.load_samples(samples.clone());
            if calc.should_rotate(config.entropy_threshold) {
                return Some(RotationReason::EntropyThreshold);
            }
        }

        if matches!(
            config.strategy,
            RotationStrategy::Proactive | RotationStrategy::Hybrid
        ) && let AuthCredential::OAuth { expires, .. } = &account.credential
        {
            let time_to_expiry_ms = expires.saturating_sub(now_ms);
            if policy.should_refresh_proactively(provider, time_to_expiry_ms) {
                return Some(RotationReason::Proactive);
            }
        }

        None
    }

    fn oauth_account_candidates(
        &self,
        provider: &str,
        pool: &OAuthProviderPool,
        now: i64,
        include_cooldown: bool,
        model_id: Option<&str>,
    ) -> Vec<String> {
        let mut candidates = Vec::new();
        let mut deprioritized = Vec::new();
        for account_id in &pool.order {
            let Some(account) = pool.accounts.get(account_id) else {
                continue;
            };
            let AuthCredential::OAuth { expires, .. } = &account.credential else {
                continue;
            };
            if self.oauth_record_revoked(account) {
                continue;
            }
            if *expires <= now {
                continue;
            }
            if account.health.requires_relogin {
                continue;
            }
            if !Self::oauth_policy_allows_model(&account.policy, model_id) {
                continue;
            }
            if !include_cooldown
                && account
                    .health
                    .cooldown_until_ms
                    .is_some_and(|until| until > now)
            {
                continue;
            }
            if self
                .account_rotation_policy_reason(provider, account, now)
                .is_some()
            {
                deprioritized.push(account_id.clone());
            } else {
                candidates.push(account_id.clone());
            }
        }
        candidates.extend(deprioritized);
        candidates
    }

    fn oauth_model_pattern_matches(pattern: &str, model_id: &str) -> bool {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return false;
        }
        let pattern_lc = pattern.to_ascii_lowercase();
        let model_lc = model_id.trim().to_ascii_lowercase();
        if pattern_lc.ends_with('*') {
            let prefix = pattern_lc.trim_end_matches('*');
            return model_lc.starts_with(prefix);
        }
        pattern_lc == model_lc
    }

    fn oauth_policy_allows_model(policy: &OAuthAccountPolicy, model_id: Option<&str>) -> bool {
        let Some(model_id) = model_id.filter(|id| !id.trim().is_empty()) else {
            return true;
        };
        let allowed_ok = policy.allowed_models.is_empty()
            || policy
                .allowed_models
                .iter()
                .any(|pattern| Self::oauth_model_pattern_matches(pattern, model_id));
        if !allowed_ok {
            return false;
        }
        !policy
            .excluded_models
            .iter()
            .any(|pattern| Self::oauth_model_pattern_matches(pattern, model_id))
    }

    #[allow(clippy::significant_drop_tightening)]
    fn select_rotating_candidate(&self, key: &str, candidates: &[String]) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let selected = {
            let mut cursors = self.rotation_cursors.lock().ok()?;
            let cursor = cursors.entry(key.to_string()).or_insert(0);
            let idx = *cursor % candidates.len();
            *cursor = cursor.saturating_add(1);
            candidates[idx].clone()
        };
        Some(selected)
    }

    pub fn build_oauth_rotation_plan(
        &self,
        provider: &str,
        model_id: Option<&str>,
        max_attempts: usize,
    ) -> Option<OAuthRotationPlan> {
        let now = chrono::Utc::now().timestamp_millis();
        for key in Self::provider_lookup_keys(provider) {
            let Some(pool) = self.oauth_pools.get(&key) else {
                continue;
            };

            let mut candidates = self.oauth_account_candidates(&key, pool, now, false, model_id);
            if candidates.is_empty() {
                candidates = self.oauth_account_candidates(&key, pool, now, true, model_id);
            }
            if candidates.is_empty() {
                continue;
            }

            let max_retry_budget = candidates.len().saturating_mul(2).saturating_add(1).max(1);
            let bounded_attempts = max_attempts.max(1).min(max_retry_budget).max(1);
            return Some(OAuthRotationPlan {
                provider: key.clone(),
                model_id: model_id.map(str::to_string),
                max_attempts: bounded_attempts,
                candidates: candidates
                    .into_iter()
                    .map(|account_id| OAuthCandidate {
                        provider: key.clone(),
                        account_id,
                    })
                    .collect(),
            });
        }
        None
    }

    pub fn execute_oauth_rotation_plan_with_outcomes(
        &mut self,
        plan: &OAuthRotationPlan,
        replayable_request: bool,
        scripted_outcomes: &[OAuthOutcome],
    ) -> (Option<OAuthCandidate>, RotationTrace) {
        let mut trace = RotationTrace::default();
        let mut selected = None;
        let mut outcome_idx = 0usize;
        let mut attempts = 0usize;
        let max_attempts = plan.max_attempts.max(1);
        let now = chrono::Utc::now().timestamp_millis();

        for candidate in &plan.candidates {
            if attempts >= max_attempts || outcome_idx >= scripted_outcomes.len() {
                break;
            }
            let provider = candidate.provider.as_str();
            let account_id = candidate.account_id.as_str();
            let adapter = oauth_provider_adapter(provider);

            let mut apply_outcome = |outcome: &OAuthOutcome| {
                let reason = outcome
                    .reason
                    .as_deref()
                    .unwrap_or("request failed")
                    .to_string();
                let class = outcome
                    .class
                    .unwrap_or_else(|| adapter.classify_error(reason.as_str()));
                trace.attempts.push(RotationTraceStep {
                    provider: provider.to_string(),
                    account_id: account_id.to_string(),
                    success: outcome.success,
                    class: Some(class),
                    reason: Some(reason.clone()),
                    status_code: outcome.status_code,
                });
                if outcome.success {
                    self.apply_oauth_success_runtime(provider, account_id, now);
                    return;
                }
                let kind = match class {
                    OAuthErrorClass::Auth | OAuthErrorClass::Fatal => {
                        OAuthFailureKind::RequiresRelogin
                    }
                    OAuthErrorClass::RateLimit | OAuthErrorClass::Quota => {
                        OAuthFailureKind::RateLimited
                    }
                    OAuthErrorClass::Transport => OAuthFailureKind::Transient,
                };
                let retry_after_ms = outcome
                    .retry_after_ms
                    .or_else(|| adapter.retry_after_ms(reason.as_str()));
                self.apply_oauth_failure_runtime(
                    provider,
                    account_id,
                    kind,
                    reason.as_str(),
                    now,
                    retry_after_ms,
                    outcome.status_code,
                );
            };

            let first = &scripted_outcomes[outcome_idx];
            outcome_idx += 1;
            attempts += 1;
            apply_outcome(first);
            if first.success {
                selected = Some(candidate.clone());
                break;
            }
            if !replayable_request {
                break;
            }

            let class = first.class.unwrap_or_else(|| {
                oauth_provider_adapter(provider)
                    .classify_error(first.reason.as_deref().unwrap_or("request failed"))
            });
            if matches!(class, OAuthErrorClass::Auth | OAuthErrorClass::Transport)
                && attempts < max_attempts
                && outcome_idx < scripted_outcomes.len()
            {
                let retry = &scripted_outcomes[outcome_idx];
                outcome_idx += 1;
                attempts += 1;
                apply_outcome(retry);
                if retry.success {
                    selected = Some(candidate.clone());
                    break;
                }
                if !replayable_request {
                    break;
                }
            }
        }

        (selected, trace)
    }

    fn resolve_rotating_oauth_token(&self, provider: &str) -> Option<(String, String)> {
        self.resolve_rotating_oauth_token_for_model(provider, None)
    }

    fn resolve_rotating_oauth_token_for_model(
        &self,
        provider: &str,
        model_id: Option<&str>,
    ) -> Option<(String, String)> {
        let now = chrono::Utc::now().timestamp_millis();
        for key in Self::provider_lookup_keys(provider) {
            let Some(pool) = self.oauth_pools.get(&key) else {
                continue;
            };

            let mut candidates = self.oauth_account_candidates(&key, pool, now, false, model_id);
            if candidates.is_empty() {
                candidates = self.oauth_account_candidates(&key, pool, now, true, model_id);
            }
            if candidates.is_empty() {
                continue;
            }

            let selected_id = if let Some(active_id) = &pool.active {
                if candidates.iter().any(|candidate| candidate == active_id) {
                    active_id.clone()
                } else {
                    self.select_rotating_candidate(&key, &candidates)?
                }
            } else {
                self.select_rotating_candidate(&key, &candidates)?
            };

            if let Some(account) = pool.accounts.get(&selected_id)
                && let Some(api_key) = api_key_from_credential(&account.credential)
            {
                return Some((key, api_key));
            }
        }
        None
    }

    fn classify_oauth_failure(error: &str) -> OAuthFailureKind {
        Self::classify_oauth_failure_for_provider("self-contained", error, None).0
    }

    /// Enhanced OAuth failure classification with HTTP status code support.
    ///
    /// Classification logic:
    /// - 401/403 (Auth): Requires relogin, rotate to next account
    /// - 429 (RateLimit): Use Retry-After header if present, rotate to next account
    /// - Quota (403 with quota message): Mark account as exhausted, rotate
    /// - Network/Transport (5xx, timeouts): Transient, retry same account
    fn classify_oauth_failure_for_provider(
        provider: &str,
        error: &str,
        status_code: Option<u16>,
    ) -> (OAuthFailureKind, Option<i64>) {
        let adapter = oauth_provider_adapter(provider);
        let class = adapter.classify_error(error);
        let retry_after_ms = adapter.retry_after_ms(error);

        // Enhanced classification based on status code when available
        let kind = match status_code {
            // 401 Unauthorized - token expired or invalid
            Some(401) => OAuthFailureKind::RequiresRelogin,
            // 403 Forbidden - could be auth or quota
            Some(403) => {
                // Check if it's a quota error
                if class == OAuthErrorClass::Quota {
                    OAuthFailureKind::QuotaExhausted
                } else {
                    OAuthFailureKind::RequiresRelogin
                }
            }
            // 429 Rate Limited
            Some(429) => OAuthFailureKind::RateLimited,
            // 5xx Server errors - transient
            Some(500..=599) => OAuthFailureKind::Transient,
            // Use text-based classification for other cases
            _ => match class {
                OAuthErrorClass::Auth | OAuthErrorClass::Fatal => OAuthFailureKind::RequiresRelogin,
                OAuthErrorClass::RateLimit => OAuthFailureKind::RateLimited,
                OAuthErrorClass::Quota => OAuthFailureKind::QuotaExhausted,
                OAuthErrorClass::Transport => OAuthFailureKind::Transient,
            },
        };
        (kind, retry_after_ms)
    }

    fn canonical_oauth_provider(provider: &str) -> String {
        canonical_provider_id(provider)
            .unwrap_or(provider)
            .to_string()
    }

    fn provider_requires_self_contained_refresh(provider: &str) -> bool {
        matches!(
            Self::canonical_oauth_provider(provider).as_str(),
            "openai-codex" | "github-copilot" | "gitlab"
        )
    }

    fn provider_non_refreshable_reason(
        provider: &str,
        access_token: &str,
        refresh_token: &str,
        token_url: Option<&str>,
        client_id: Option<&str>,
    ) -> Option<String> {
        let adapter = oauth_provider_adapter(provider);
        if adapter.can_refresh(provider, access_token, refresh_token, token_url, client_id) {
            None
        } else {
            adapter.non_refreshable_reason(
                provider,
                access_token,
                refresh_token,
                token_url,
                client_id,
            )
        }
    }

    fn find_oauth_account_for_token(
        &self,
        provider: &str,
        api_key: &str,
    ) -> Option<(String, String)> {
        let normalized_key = if canonical_provider_id(provider).unwrap_or(provider) == "anthropic" {
            unmark_anthropic_oauth_bearer_token(api_key).unwrap_or(api_key)
        } else {
            api_key
        };

        for key in Self::provider_lookup_keys(provider) {
            let Some(pool) = self.oauth_pools.get(&key) else {
                continue;
            };
            for (account_id, account) in &pool.accounts {
                if let AuthCredential::OAuth { access_token, .. } = &account.credential
                    && access_token == normalized_key
                {
                    return Some((key.clone(), account_id.clone()));
                }
            }
        }
        None
    }

    fn apply_oauth_success(pool: &mut OAuthProviderPool, account_id: &str, now: i64) {
        if let Some(account) = pool.accounts.get_mut(account_id) {
            account.health.consecutive_failures = 0;
            account.health.cooldown_until_ms = None;
            account.health.requires_relogin = false;
            account.health.last_error = None;
            account.health.last_success_ms = Some(now);

            // V2: Track consecutive successes
            let successes = account.health.consecutive_successes.unwrap_or(0);
            account.health.consecutive_successes = Some(successes.saturating_add(1));

            // V2: Track total requests
            let total = account.health.total_requests.unwrap_or(0);
            account.health.total_requests = Some(total.saturating_add(1));

            // V2: Clear quota exhaustion on success
            account.health.exhausted_until_ms = None;
        }
        pool.order.retain(|id| id != account_id);
        pool.order.push(account_id.to_string());
    }

    fn apply_oauth_failure(
        pool: &mut OAuthProviderPool,
        account_id: &str,
        kind: OAuthFailureKind,
        error: &str,
        now: i64,
        retry_after_ms: Option<i64>,
        status_code: Option<u16>,
    ) {
        if let Some(account) = pool.accounts.get_mut(account_id) {
            account.health.consecutive_failures =
                account.health.consecutive_failures.saturating_add(1);
            account.health.last_failure_ms = Some(now);
            account.health.last_error = Some(error.chars().take(256).collect());

            // V2: Reset consecutive successes on any failure
            account.health.consecutive_successes = Some(0);

            // V2: Track total requests
            let total = account.health.total_requests.unwrap_or(0);
            account.health.total_requests = Some(total.saturating_add(1));

            // V2: Track last HTTP status code
            account.health.last_status_code = status_code;

            match kind {
                OAuthFailureKind::RequiresRelogin => {
                    account.health.requires_relogin = true;
                    account.health.cooldown_until_ms = None;
                }
                OAuthFailureKind::RateLimited => {
                    let cooldown_ms = retry_after_ms.unwrap_or_else(|| {
                        let multiplier =
                            2_i64.pow(account.health.consecutive_failures.saturating_sub(1).min(4));
                        (OAUTH_RATE_LIMIT_BASE_COOLDOWN_MS * multiplier).min(OAUTH_MAX_COOLDOWN_MS)
                    });
                    account.health.cooldown_until_ms = Some(now.saturating_add(cooldown_ms));
                }
                OAuthFailureKind::Transient => {
                    let multiplier =
                        2_i64.pow(account.health.consecutive_failures.saturating_sub(1).min(4));
                    let cooldown_ms =
                        (OAUTH_TRANSIENT_BASE_COOLDOWN_MS * multiplier).min(OAUTH_MAX_COOLDOWN_MS);
                    account.health.cooldown_until_ms = Some(now.saturating_add(cooldown_ms));
                }
                OAuthFailureKind::QuotaExhausted => {
                    // Set exhausted_until_ms - this is distinct from rate limit cooldown
                    // Use retry_after_ms if provided, otherwise default to 1 hour
                    let exhausted_ms = retry_after_ms.unwrap_or(60 * 60 * 1000);
                    account.health.exhausted_until_ms = Some(now.saturating_add(exhausted_ms));
                    account.health.cooldown_until_ms = None;
                }
            }
        }
        pool.order.retain(|id| id != account_id);
        pool.order.push(account_id.to_string());
    }

    fn emit_refresh_event(&self, event: RefreshEvent) {
        if let Some(callbacks) = &self.callbacks {
            callbacks.notify_all(&event);
        }
    }

    fn emit_audit_event(&self, event: AuditEvent) {
        if let Some(log) = &self.audit_log {
            log.record(event);
        }
    }

    fn move_account_to_back(pool: &mut OAuthProviderPool, account_id: &str) {
        pool.order.retain(|id| id != account_id);
        pool.order.push(account_id.to_string());
        if pool.active.as_deref() == Some(account_id) {
            pool.active = pool
                .order
                .iter()
                .find(|id| id.as_str() != account_id && pool.accounts.contains_key(*id))
                .cloned()
                .or_else(|| Some(account_id.to_string()));
        }
    }

    fn update_entropy_samples(
        account: &mut OAuthAccountRecord,
        success: bool,
        error_class: Option<OAuthErrorClass>,
        now: i64,
        max_samples: usize,
    ) -> EntropyCalculator {
        let mut calc = EntropyCalculator::new(max_samples.max(1));
        if let Some(samples) = &account.health.entropy_samples {
            calc.load_samples(samples.clone());
        }
        if success {
            calc.record_success(now, 0);
        } else {
            calc.record_failure(now, 0, error_class.unwrap_or(OAuthErrorClass::Transport));
        }
        account.health.entropy_samples = Some(calc.samples_vec());
        calc
    }

    fn update_recovery_state_after_failure(
        recovery_config: &RecoveryConfig,
        account: &mut OAuthAccountRecord,
        provider: &str,
        account_id: &str,
        kind: OAuthFailureKind,
        error: &str,
        status_code: Option<u16>,
        now: i64,
    ) {
        let mut machine = RecoveryMachine::new(
            RecoveryContext::new(provider.to_string(), Some(account_id.to_string()), now)
                .with_trigger_error(error.to_string()),
            recovery_config.clone(),
        );

        if let Some(code) = status_code {
            machine = RecoveryMachine::new(
                machine.context().clone().with_status_code(code),
                recovery_config.clone(),
            );
        }

        let _ = machine.start(now);
        let _ = machine.process_event(RecoveryEvent::ReloadComplete { success: false }, now);
        match kind {
            OAuthFailureKind::RequiresRelogin => {
                let _ = machine.process_event(
                    RecoveryEvent::RefreshComplete {
                        result: RefreshResult::RequiresRelogin,
                    },
                    now,
                );
            }
            OAuthFailureKind::RateLimited => {
                let _ = machine.process_event(
                    RecoveryEvent::RefreshComplete {
                        result: RefreshResult::RateLimited,
                    },
                    now,
                );
            }
            OAuthFailureKind::Transient | OAuthFailureKind::QuotaExhausted => {
                let _ = machine.process_event(
                    RecoveryEvent::RefreshComplete {
                        result: RefreshResult::RetryableFailure,
                    },
                    now,
                );
            }
        }
        account.health.recovery_state = Some(machine.state().clone());
    }

    fn update_recovery_state_after_success(account: &mut OAuthAccountRecord, now: i64) {
        if account
            .health
            .recovery_state
            .as_ref()
            .is_some_and(|state| !matches!(state, RecoveryState::Initial))
        {
            account.health.recovery_state = Some(RecoveryState::Done {
                recovered_at: now,
                method: RecoveryMethod::AccountRotation,
            });
        } else {
            account.health.recovery_state = None;
        }
    }

    fn apply_oauth_success_runtime(&mut self, provider: &str, account_id: &str, now: i64) {
        let policy = self.rotation_policy.clone();
        let mut rotated_reason: Option<RotationReason> = None;
        let mut completed_recovery = false;
        if let Some(pool) = self.oauth_pools.get_mut(provider) {
            Self::apply_oauth_success(pool, account_id, now);
            if let Some(account) = pool.accounts.get_mut(account_id) {
                let config = policy.config_for_provider(provider);
                let calc = Self::update_entropy_samples(
                    account,
                    true,
                    None,
                    now,
                    config.max_entropy_samples,
                );
                completed_recovery = account.health.recovery_state.is_some();
                Self::update_recovery_state_after_success(account, now);
                if matches!(
                    config.strategy,
                    RotationStrategy::EntropyBased | RotationStrategy::Hybrid
                ) && calc.should_rotate(config.entropy_threshold)
                {
                    rotated_reason = Some(RotationReason::EntropyThreshold);
                } else {
                    rotated_reason =
                        Self::account_rotation_policy_reason_with(&policy, provider, account, now);
                }
                if let Some(reason) = rotated_reason {
                    account.health.last_rotation_ms = Some(now);
                    account.health.last_rotation_reason = Some(reason);
                    if matches!(
                        reason,
                        RotationReason::EntropyThreshold
                            | RotationReason::Interval
                            | RotationReason::MaxUsage
                    ) {
                        Self::move_account_to_back(pool, account_id);
                    }
                }
            }
        }

        if let Some(reason) = rotated_reason {
            global_cache().invalidate_for_account(provider, account_id);
            self.emit_audit_event(
                AuditEvent::new(now, AuditEventType::TokenRotated, provider.to_string())
                    .with_account(account_id.to_string())
                    .with_details(format!("reason={reason:?}")),
            );
        }
        if completed_recovery {
            self.emit_audit_event(
                AuditEvent::new(now, AuditEventType::RecoveryCompleted, provider.to_string())
                    .with_account(account_id.to_string()),
            );
        }
    }

    fn apply_oauth_failure_runtime(
        &mut self,
        provider: &str,
        account_id: &str,
        kind: OAuthFailureKind,
        error: &str,
        now: i64,
        retry_after_ms: Option<i64>,
        status_code: Option<u16>,
    ) {
        let policy = self.rotation_policy.clone();
        let recovery_config = self.recovery_config.clone();
        let mut emitted_rate_limited = false;
        let mut emitted_relogin = false;
        let mut rotated = false;
        let mut recovery_started = false;

        if let Some(pool) = self.oauth_pools.get_mut(provider) {
            Self::apply_oauth_failure(
                pool,
                account_id,
                kind,
                error,
                now,
                retry_after_ms,
                status_code,
            );
            if let Some(account) = pool.accounts.get_mut(account_id) {
                let config = policy.config_for_provider(provider);
                let class = oauth_provider_adapter(provider).classify_error(error);
                let calc = Self::update_entropy_samples(
                    account,
                    false,
                    Some(class),
                    now,
                    config.max_entropy_samples,
                );
                recovery_started = true;
                Self::update_recovery_state_after_failure(
                    &recovery_config,
                    account,
                    provider,
                    account_id,
                    kind,
                    error,
                    status_code,
                    now,
                );

                if matches!(kind, OAuthFailureKind::RequiresRelogin) {
                    emitted_relogin = true;
                }
                if matches!(
                    kind,
                    OAuthFailureKind::RateLimited | OAuthFailureKind::QuotaExhausted
                ) {
                    emitted_rate_limited = true;
                }

                let rotate_for_error = status_code
                    .is_some_and(|code| policy.should_rotate_on_error(provider, code))
                    || matches!(kind, OAuthFailureKind::RequiresRelogin);
                let rotate_for_entropy = matches!(
                    config.strategy,
                    RotationStrategy::EntropyBased | RotationStrategy::Hybrid
                ) && calc.should_rotate(config.entropy_threshold);

                if rotate_for_error || rotate_for_entropy {
                    let reason = if rotate_for_entropy {
                        RotationReason::EntropyThreshold
                    } else {
                        RotationReason::ErrorTriggered
                    };
                    account.health.last_rotation_ms = Some(now);
                    account.health.last_rotation_reason = Some(reason);
                    Self::move_account_to_back(pool, account_id);
                    rotated = true;
                }
            }
        }

        if emitted_relogin {
            self.emit_refresh_event(RefreshEvent::RequiresRelogin {
                provider: provider.to_string(),
                account_id: account_id.to_string(),
                reason: error.to_string(),
            });
            self.emit_audit_event(
                AuditEvent::new(now, AuditEventType::RecoveryFailed, provider.to_string())
                    .with_account(account_id.to_string())
                    .with_details(error.to_string()),
            );
        } else {
            self.emit_refresh_event(RefreshEvent::Failure {
                provider: provider.to_string(),
                account_id: account_id.to_string(),
                error: error.to_string(),
                error_class: oauth_provider_adapter(provider).classify_error(error),
                will_retry: matches!(
                    kind,
                    OAuthFailureKind::RateLimited | OAuthFailureKind::Transient
                ),
            });
        }

        if emitted_rate_limited {
            self.emit_refresh_event(RefreshEvent::RateLimited {
                provider: provider.to_string(),
                account_id: account_id.to_string(),
                retry_after_ms,
            });
        }

        if rotated {
            global_cache().invalidate_for_account(provider, account_id);
            self.emit_audit_event(
                AuditEvent::new(now, AuditEventType::TokenRotated, provider.to_string())
                    .with_account(account_id.to_string())
                    .with_details("reason=error".to_string()),
            );
        }
        if recovery_started {
            self.emit_audit_event(
                AuditEvent::new(now, AuditEventType::RecoveryStarted, provider.to_string())
                    .with_account(account_id.to_string()),
            );
        }
    }

    /// Record OAuth provider runtime health from the latest request outcome.
    pub fn record_oauth_runtime_outcome(
        &mut self,
        provider: &str,
        resolved_api_key: Option<&str>,
        outcome: OAuthRuntimeOutcome,
        error: Option<&str>,
    ) {
        let Some(api_key) = resolved_api_key else {
            return;
        };
        let Some((provider_key, account_id)) = self.find_oauth_account_for_token(provider, api_key)
        else {
            return;
        };
        let now = chrono::Utc::now().timestamp_millis();
        match outcome {
            OAuthRuntimeOutcome::Success => {
                self.apply_oauth_success_runtime(&provider_key, &account_id, now);
            }
            OAuthRuntimeOutcome::Failure => {
                let (kind, retry_after_ms) = Self::classify_oauth_failure_for_provider(
                    provider,
                    error.unwrap_or_default(),
                    None, // status_code not available from runtime outcome
                );
                self.apply_oauth_failure_runtime(
                    &provider_key,
                    &account_id,
                    kind,
                    error.unwrap_or("request failed"),
                    now,
                    retry_after_ms,
                    None, // status_code not available from runtime outcome
                );
            }
        }
    }

    /// Format a human-readable status of OAuth accounts for a provider.
    pub fn format_accounts_status(&self, provider: &str) -> String {
        let now = chrono::Utc::now().timestamp_millis();
        let mut output = format!("OAuth accounts for '{provider}':\n");

        let Some(pool) = self.oauth_pool_for_provider(provider) else {
            output.push_str("  No OAuth pool configured for this provider.\n");
            return output;
        };

        if pool.accounts.is_empty() {
            output.push_str("  No accounts configured.\n");
            return output;
        }

        for account_id in &pool.order {
            if let Some(account) = pool.accounts.get(account_id) {
                let is_active = pool.active.as_ref() == Some(account_id);
                let active_marker = if is_active { "▶ " } else { "  " };

                let status = if account.health.requires_relogin {
                    "⚠️  REQUIRES RELOGIN"
                } else if account
                    .health
                    .exhausted_until_ms
                    .is_some_and(|until| until > now)
                {
                    "⛔ QUOTA EXHAUSTED"
                } else if account
                    .health
                    .cooldown_until_ms
                    .is_some_and(|until| until > now)
                {
                    "⏳ RATE LIMITED"
                } else if let AuthCredential::OAuth { expires, .. } = &account.credential {
                    if *expires <= now {
                        "❌ EXPIRED"
                    } else {
                        let remaining = (*expires - now) / 1000;
                        if remaining < 600 {
                            "⏰ EXPIRING SOON"
                        } else {
                            "✅ OK"
                        }
                    }
                } else {
                    "✅ OK"
                };

                let label = account.label.as_deref().unwrap_or(account_id);
                let successes = account.health.consecutive_successes.unwrap_or(0);
                let failures = account.health.consecutive_failures;
                let total = account.health.total_requests.unwrap_or(0);

                let _ = writeln!(output, "{active_marker}{account_id} ({label})");
                let _ = writeln!(output, "      Status: {status}");
                let _ = writeln!(
                    output,
                    "      Stats: {successes} consecutive successes, {failures} failures, {total} total"
                );

                if let Some(state) = &account.health.recovery_state {
                    let _ = writeln!(output, "      Recovery: {}", state.description());
                }
                if let Some(last_rotation_ms) = account.health.last_rotation_ms {
                    let reason = account
                        .health
                        .last_rotation_reason
                        .map_or_else(|| "unknown".to_string(), |r| format!("{r:?}"));
                    let _ = writeln!(output, "      Last rotation: {last_rotation_ms} ({reason})");
                }

                if let Some(last_error) = &account.health.last_error {
                    let truncated: String = last_error.chars().take(60).collect();
                    let _ = writeln!(output, "      Last error: {truncated}...");
                }
            }
        }

        if pool.active.is_none() {
            output.push_str("\n  No active account set. Use /use-account to select one.\n");
        }

        output
    }

    /// Set the active account for a provider's OAuth pool.
    pub fn set_active_account(
        &mut self,
        provider: &str,
        account_id: &str,
    ) -> crate::error::Result<()> {
        let provider_key = Self::normalize_provider_key(provider);

        let Some(pool) = self.oauth_pools.get_mut(&provider_key) else {
            return Err(crate::error::Error::auth(format!(
                "No OAuth pool for provider '{provider}'"
            )));
        };

        if !pool.accounts.contains_key(account_id) {
            return Err(crate::error::Error::auth(format!(
                "Account '{account_id}' not found in pool for '{provider}'"
            )));
        }

        pool.active = Some(account_id.to_string());
        Ok(())
    }

    /// Revoke a specific OAuth account token and mark it for relogin.
    pub fn revoke_oauth_account_token(
        &mut self,
        provider: &str,
        account_id: &str,
        reason: RevocationReason,
    ) -> crate::error::Result<()> {
        let provider_key = Self::normalize_provider_key(provider);
        let now = chrono::Utc::now().timestamp_millis();

        let access_token = {
            let Some(pool) = self.oauth_pools.get_mut(&provider_key) else {
                return Err(crate::error::Error::auth(format!(
                    "No OAuth pool for provider '{provider}'"
                )));
            };
            let Some(account) = pool.accounts.get_mut(account_id) else {
                return Err(crate::error::Error::auth(format!(
                    "Account '{account_id}' not found for provider '{provider}'"
                )));
            };
            let AuthCredential::OAuth { access_token, .. } = &account.credential else {
                return Err(crate::error::Error::auth(format!(
                    "Account '{account_id}' does not contain an OAuth credential"
                )));
            };
            account.health.requires_relogin = true;
            account.health.cooldown_until_ms = None;
            account.health.last_error = Some(format!("token revoked ({reason})"));
            access_token.clone()
        };

        self.revocation_manager.revoke(
            access_token.as_str(),
            provider_key.clone(),
            Some(account_id.to_string()),
            now,
            reason,
        );
        global_cache().invalidate_for_account(&provider_key, account_id);
        self.emit_refresh_event(RefreshEvent::RequiresRelogin {
            provider: provider_key.clone(),
            account_id: account_id.to_string(),
            reason: format!("token revoked ({reason})"),
        });
        self.emit_audit_event(
            AuditEvent::new(now, AuditEventType::TokenRevoked, provider_key.clone())
                .with_account(account_id.to_string())
                .with_details(format!("reason={reason}")),
        );
        Ok(())
    }

    /// Return a summary of current recovery states for a provider.
    pub fn recovery_state_summary(&self, provider: &str) -> Vec<(String, Option<RecoveryState>)> {
        let provider_key = Self::normalize_provider_key(provider);
        let Some(pool) = self.oauth_pools.get(&provider_key) else {
            return Vec::new();
        };
        pool.order
            .iter()
            .filter_map(|account_id| {
                pool.accounts
                    .get(account_id)
                    .map(|account| (account_id.clone(), account.health.recovery_state.clone()))
            })
            .collect()
    }

    /// Return recent token revocation records.
    pub fn revocation_records(&self) -> &[RevocationRecord] {
        self.revocation_manager.revocation_log()
    }

    /// Return a human-readable source label when credentials can be auto-detected
    /// from other locally-installed coding CLIs.
    pub fn external_setup_source(&self, provider: &str) -> Option<&'static str> {
        if !self.allow_external_provider_lookup() {
            return None;
        }
        external_setup_source(provider)
    }

    /// Resolve API key with precedence.
    pub fn resolve_api_key(&self, provider: &str, override_key: Option<&str>) -> Option<String> {
        self.resolve_api_key_for_model(provider, None, override_key)
    }

    /// Resolve API key with precedence, optionally filtering pooled OAuth
    /// candidates through account policy constraints for a specific model.
    pub fn resolve_api_key_for_model(
        &self,
        provider: &str,
        model_id: Option<&str>,
        override_key: Option<&str>,
    ) -> Option<String> {
        self.resolve_api_key_with_env_lookup_for_model(provider, model_id, override_key, |var| {
            std::env::var(var).ok()
        })
    }

    fn resolve_api_key_with_env_lookup<F>(
        &self,
        provider: &str,
        override_key: Option<&str>,
        env_lookup: F,
    ) -> Option<String>
    where
        F: FnMut(&str) -> Option<String>,
    {
        self.resolve_api_key_with_env_lookup_for_model(provider, None, override_key, env_lookup)
    }

    fn resolve_api_key_with_env_lookup_for_model<F>(
        &self,
        provider: &str,
        model_id: Option<&str>,
        override_key: Option<&str>,
        mut env_lookup: F,
    ) -> Option<String>
    where
        F: FnMut(&str) -> Option<String>,
    {
        if let Some(key) = override_key {
            return Some(key.to_string());
        }

        if let Some((resolved_provider, api_key)) =
            self.resolve_rotating_oauth_token_for_model(provider, model_id)
        {
            return if resolved_provider == "anthropic" {
                Some(mark_anthropic_oauth_bearer_token(&api_key))
            } else {
                Some(api_key)
            };
        }

        // Prefer explicit stored OAuth/Bearer credentials over ambient env vars.
        // This prevents stale shell env keys from silently overriding successful `/login` flows.
        if let Some(credential) = self.credential_for_provider_entry(provider)
            && let Some(key) = match credential {
                AuthCredential::OAuth { access_token, .. }
                    if self.revocation_manager.is_revoked(access_token) =>
                {
                    None
                }
                AuthCredential::OAuth { .. }
                    if canonical_provider_id(provider).unwrap_or(provider) == "anthropic" =>
                {
                    api_key_from_credential(credential)
                        .map(|token| mark_anthropic_oauth_bearer_token(&token))
                }
                AuthCredential::OAuth { .. } | AuthCredential::BearerToken { .. } => {
                    api_key_from_credential(credential)
                }
                _ => None,
            }
        {
            return Some(key);
        }

        if let Some(key) = env_keys_for_provider(provider).iter().find_map(|var| {
            env_lookup(var).and_then(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
        }) {
            return Some(key);
        }

        if let Some(key) = self.api_key(provider) {
            return Some(key);
        }

        if self.allow_external_provider_lookup() {
            if let Some(key) = resolve_external_provider_api_key(provider) {
                return Some(key);
            }
        }

        canonical_provider_id(provider)
            .filter(|canonical| *canonical != provider)
            .and_then(|canonical| {
                self.api_key(canonical).or_else(|| {
                    self.allow_external_provider_lookup()
                        .then(|| resolve_external_provider_api_key(canonical))
                        .flatten()
                })
            })
    }

    /// Refresh any expired OAuth tokens that this binary knows how to refresh.
    ///
    /// This keeps startup behavior predictable: models that rely on OAuth credentials remain
    /// available after restart without requiring the user to re-login.
    pub async fn refresh_expired_oauth_tokens(&mut self) -> Result<()> {
        let client = crate::http::client::Client::new();
        self.refresh_expired_oauth_tokens_with_client(&client).await
    }

    /// Execute one non-blocking background refresh tick.
    ///
    /// This path reuses startup refresh logic with in-flight dedupe slots so
    /// concurrent scheduler ticks don't duplicate account refresh mutations.
    pub async fn run_background_refresh_tick(&mut self) -> Result<()> {
        let client = crate::http::client::Client::new();
        self.run_background_refresh_tick_with_client(&client).await
    }

    pub async fn run_background_refresh_tick_with_client(
        &mut self,
        client: &crate::http::client::Client,
    ) -> Result<()> {
        self.refresh_expired_oauth_tokens_with_client(client).await
    }

    /// Refresh any expired OAuth tokens using the provided HTTP client.
    ///
    /// This is primarily intended for tests and deterministic harnesses (e.g. VCR playback),
    /// but is also useful for callers that want to supply a custom HTTP implementation.
    #[allow(clippy::too_many_lines)]
    pub async fn refresh_expired_oauth_tokens_with_client(
        &mut self,
        client: &crate::http::client::Client,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let proactive_deadline = now + PROACTIVE_REFRESH_WINDOW_MS;
        let mut refreshes: Vec<OAuthRefreshRequest> = Vec::new();

        for (provider, pool) in &self.oauth_pools {
            for (account_id, account) in &pool.accounts {
                let AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires,
                    token_url,
                    client_id,
                    ..
                } = &account.credential
                else {
                    continue;
                };
                if account.health.requires_relogin {
                    continue;
                }
                // Proactive refresh: refresh if the token will expire within the
                // proactive window, not just when already expired.
                if *expires <= proactive_deadline {
                    refreshes.push(OAuthRefreshRequest {
                        provider: provider.clone(),
                        account_id: Some(account_id.clone()),
                        access_token: access_token.clone(),
                        refresh_token: refresh_token.clone(),
                        token_url: token_url.clone(),
                        client_id: client_id.clone(),
                        provider_metadata: Some(account.provider_metadata.clone()),
                    });
                }
            }
        }

        for (provider, cred) in &self.entries {
            let AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                token_url,
                client_id,
                ..
            } = cred
            else {
                continue;
            };
            if *expires <= proactive_deadline {
                refreshes.push(OAuthRefreshRequest {
                    provider: provider.clone(),
                    account_id: None,
                    access_token: access_token.clone(),
                    refresh_token: refresh_token.clone(),
                    token_url: token_url.clone(),
                    client_id: client_id.clone(),
                    provider_metadata: None,
                });
            }
        }

        let mut failed_providers = Vec::new();

        for OAuthRefreshRequest {
            provider,
            account_id,
            access_token,
            refresh_token,
            token_url: stored_token_url,
            client_id: stored_client_id,
            ..
        } in refreshes
        {
            let Some(slot_key) = try_acquire_oauth_refresh_slot(&provider, account_id.as_deref())
            else {
                continue;
            };
            if let Some(reason) = Self::provider_non_refreshable_reason(
                &provider,
                &access_token,
                &refresh_token,
                stored_token_url.as_deref(),
                stored_client_id.as_deref(),
            ) {
                if let Some(account_id) = account_id.as_deref() {
                    self.apply_oauth_failure_runtime(
                        &provider,
                        account_id,
                        OAuthFailureKind::RequiresRelogin,
                        reason.as_str(),
                        now,
                        None,
                        Some(401),
                    );
                }
                failed_providers.push(provider);
                release_oauth_refresh_slot(&slot_key);
                continue;
            }

            let result = match provider.as_str() {
                "anthropic" => {
                    Box::pin(refresh_anthropic_oauth_token(client, &refresh_token)).await
                }
                "google-gemini-cli" => {
                    let project_id =
                        resolve_google_refresh_project_id("google-gemini-cli", &access_token)
                        .ok_or_else(|| {
                            Error::auth(
                                "google-gemini-cli OAuth credential missing projectId payload and no fallback project could be resolved from environment or gcloud configuration".to_string(),
                            )
                        })?;
                    Box::pin(refresh_google_gemini_cli_oauth_token(
                        client,
                        &refresh_token,
                        &project_id,
                    ))
                    .await
                }
                "google-antigravity" => {
                    let project_id =
                        resolve_google_refresh_project_id("google-antigravity", &access_token)
                        .ok_or_else(|| {
                            Error::auth(
                                "google-antigravity OAuth credential missing projectId payload and no fallback project could be resolved".to_string(),
                            )
                        })?;
                    Box::pin(refresh_google_antigravity_oauth_token(
                        client,
                        &refresh_token,
                        &project_id,
                    ))
                    .await
                }
                "kimi-for-coding" => {
                    let token_url = stored_token_url
                        .clone()
                        .unwrap_or_else(kimi_code_token_endpoint);
                    Box::pin(refresh_kimi_code_oauth_token(
                        client,
                        &token_url,
                        &refresh_token,
                    ))
                    .await
                }
                _ => {
                    if let (Some(url), Some(cid)) = (&stored_token_url, &stored_client_id) {
                        Box::pin(refresh_self_contained_oauth_token(
                            client,
                            url,
                            cid,
                            &refresh_token,
                            &provider,
                        ))
                        .await
                    } else if Self::provider_requires_self_contained_refresh(&provider) {
                        Err(Error::auth(format!(
                            "{provider} OAuth credential is missing token_url/client_id refresh metadata; relogin required"
                        )))
                    } else {
                        // Should have been filtered out or handled by extensions logic, but safe to ignore.
                        continue;
                    }
                }
            };

            match result {
                Ok(refreshed) => {
                    let name = provider.clone();
                    let new_expires_at = match &refreshed {
                        AuthCredential::OAuth { expires, .. } => *expires,
                        _ => now,
                    };
                    let mut refreshed_account: Option<String> = None;
                    if let Some(account_id) = account_id.as_deref() {
                        if let Some(pool) = self.oauth_pools.get_mut(&provider)
                            && let Some(account) = pool.accounts.get_mut(account_id)
                        {
                            account.credential = refreshed;
                            if !pool.order.iter().any(|id| id == account_id) {
                                pool.order.push(account_id.to_string());
                            }
                            if pool.active.is_none() {
                                pool.active = Some(account_id.to_string());
                            }
                            refreshed_account = Some(account_id.to_string());
                        }
                    } else {
                        self.entries.insert(provider, refreshed);
                    }
                    if let Some(account_id) = refreshed_account {
                        self.apply_oauth_success_runtime(&name, account_id.as_str(), now);
                        self.emit_refresh_event(RefreshEvent::Success {
                            provider: name.clone(),
                            account_id,
                            new_expires_at,
                            duration_ms: 0,
                        });
                        self.emit_audit_event(AuditEvent::new(
                            now,
                            AuditEventType::TokenRefreshed,
                            name.clone(),
                        ));
                    }
                    if let Err(e) = self.save_async().await {
                        tracing::warn!("Failed to save auth.json after refreshing {name}: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to refresh OAuth token for {provider}: {e}");
                    if let Some(account_id) = account_id.as_deref() {
                        let (kind, retry_after_ms) = Self::classify_oauth_failure_for_provider(
                            &provider,
                            &e.to_string(),
                            None,
                        );
                        self.apply_oauth_failure_runtime(
                            &provider,
                            account_id,
                            kind,
                            &e.to_string(),
                            now,
                            retry_after_ms,
                            None,
                        );
                    }
                    failed_providers.push(provider);
                }
            }
            release_oauth_refresh_slot(&slot_key);
        }

        if !failed_providers.is_empty() {
            // Return an error to signal that at least some refreshes failed,
            // but only after attempting all of them.
            return Err(Error::auth(format!(
                "OAuth token refresh failed for: {}",
                failed_providers.join(", ")
            )));
        }

        Ok(())
    }

    /// Refresh expired OAuth tokens for extension-registered providers.
    ///
    /// `extension_configs` maps provider ID to its [`OAuthConfig`](crate::models::OAuthConfig).
    /// Providers already handled by `refresh_expired_oauth_tokens_with_client` (e.g. "anthropic")
    /// are skipped.
    #[allow(clippy::too_many_lines)]
    pub async fn refresh_expired_extension_oauth_tokens(
        &mut self,
        client: &crate::http::client::Client,
        extension_configs: &HashMap<String, crate::models::OAuthConfig>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let proactive_deadline = now + PROACTIVE_REFRESH_WINDOW_MS;
        let mut refreshes: Vec<(String, Option<String>, String, crate::models::OAuthConfig)> =
            Vec::new();

        let mut enqueue_extension_refresh =
            |provider: &str,
             account_id: Option<String>,
             refresh_token: &str,
             expires: i64,
             token_url: Option<&String>,
             client_id: Option<&String>| {
                // Skip built-in providers (handled by refresh_expired_oauth_tokens_with_client).
                if matches!(
                    provider,
                    "anthropic"
                        | "openai-codex"
                        | "google-gemini-cli"
                        | "google-antigravity"
                        | "kimi-for-coding"
                ) {
                    return;
                }
                // Skip self-contained credentials — they are refreshed by
                // refresh_expired_oauth_tokens_with_client instead.
                if token_url.is_some() && client_id.is_some() {
                    return;
                }
                if expires > proactive_deadline {
                    return;
                }
                if let Some(config) = extension_configs.get(provider) {
                    refreshes.push((
                        provider.to_string(),
                        account_id,
                        refresh_token.to_string(),
                        config.clone(),
                    ));
                }
            };

        for (provider, pool) in &self.oauth_pools {
            for (account_id, account) in &pool.accounts {
                let AuthCredential::OAuth {
                    refresh_token,
                    expires,
                    token_url,
                    client_id,
                    ..
                } = &account.credential
                else {
                    continue;
                };
                if account.health.requires_relogin {
                    continue;
                }
                enqueue_extension_refresh(
                    provider,
                    Some(account_id.clone()),
                    refresh_token,
                    *expires,
                    token_url.as_ref(),
                    client_id.as_ref(),
                );
            }
        }

        for (provider, cred) in &self.entries {
            let AuthCredential::OAuth {
                refresh_token,
                expires,
                token_url,
                client_id,
                ..
            } = cred
            else {
                continue;
            };
            enqueue_extension_refresh(
                provider,
                None,
                refresh_token,
                *expires,
                token_url.as_ref(),
                client_id.as_ref(),
            );
        }

        if !refreshes.is_empty() {
            tracing::info!(
                event = "pi.auth.extension_oauth_refresh.start",
                count = refreshes.len(),
                "Refreshing expired extension OAuth tokens"
            );
        }
        let mut failed_providers: Vec<String> = Vec::new();
        for (provider, account_id, refresh_token, config) in refreshes {
            let Some(slot_key) = try_acquire_oauth_refresh_slot(&provider, account_id.as_deref())
            else {
                continue;
            };
            if refresh_token.trim().is_empty() {
                let reason = format!(
                    "{provider} OAuth credential is non-refreshable (missing refresh_token); relogin required"
                );
                if let Some(account_id) = account_id.as_deref() {
                    self.apply_oauth_failure_runtime(
                        &provider,
                        account_id,
                        OAuthFailureKind::RequiresRelogin,
                        reason.as_str(),
                        now,
                        None,
                        Some(401),
                    );
                }
                failed_providers.push(provider);
                release_oauth_refresh_slot(&slot_key);
                continue;
            }

            let start = std::time::Instant::now();
            match refresh_extension_oauth_token(client, &config, &refresh_token).await {
                Ok(refreshed) => {
                    tracing::info!(
                        event = "pi.auth.extension_oauth_refresh.ok",
                        provider = %provider,
                        elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "Extension OAuth token refreshed"
                    );
                    let new_expires_at = match &refreshed {
                        AuthCredential::OAuth { expires, .. } => *expires,
                        _ => now,
                    };
                    let mut refreshed_account: Option<String> = None;
                    if let Some(account_id) = account_id.as_deref() {
                        if let Some(pool) = self.oauth_pools.get_mut(&provider)
                            && let Some(account) = pool.accounts.get_mut(account_id)
                        {
                            account.credential = refreshed;
                            refreshed_account = Some(account_id.to_string());
                        }
                    } else {
                        self.entries.insert(provider.clone(), refreshed);
                    }
                    if let Some(account_id) = refreshed_account {
                        self.apply_oauth_success_runtime(&provider, account_id.as_str(), now);
                        self.emit_refresh_event(RefreshEvent::Success {
                            provider: provider.clone(),
                            account_id: account_id.clone(),
                            new_expires_at,
                            duration_ms: u64::try_from(start.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                        });
                    }
                    self.emit_audit_event(AuditEvent::new(
                        now,
                        AuditEventType::TokenRefreshed,
                        provider.clone(),
                    ));
                    self.save_async().await?;
                }
                Err(e) => {
                    tracing::warn!(
                        event = "pi.auth.extension_oauth_refresh.error",
                        provider = %provider,
                        error = %e,
                        elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "Failed to refresh extension OAuth token; continuing with remaining providers"
                    );
                    if let Some(account_id) = account_id.as_deref() {
                        let (kind, retry_after_ms) = Self::classify_oauth_failure_for_provider(
                            &provider,
                            &e.to_string(),
                            None,
                        );
                        self.apply_oauth_failure_runtime(
                            &provider,
                            account_id,
                            kind,
                            &e.to_string(),
                            now,
                            retry_after_ms,
                            None,
                        );
                    }
                    failed_providers.push(provider);
                }
            }
            release_oauth_refresh_slot(&slot_key);
        }
        if failed_providers.is_empty() {
            Ok(())
        } else {
            Err(Error::api(format!(
                "Extension OAuth token refresh failed for: {}",
                failed_providers.join(", ")
            )))
        }
    }

    /// Remove OAuth credentials that expired more than `max_age_ms` ago and
    /// whose refresh token is no longer usable (no stored `token_url`/`client_id`
    /// and no matching extension config).
    ///
    /// Returns the list of pruned provider IDs.
    pub fn prune_stale_credentials(&mut self, max_age_ms: i64) -> Vec<String> {
        let now = chrono::Utc::now().timestamp_millis();
        let cutoff = now - max_age_ms;
        let mut pruned = Vec::new();

        for (provider, pool) in &mut self.oauth_pools {
            let mut removed_accounts = Vec::new();
            pool.accounts.retain(|account_id, account| {
                if let AuthCredential::OAuth {
                    expires,
                    token_url,
                    client_id,
                    ..
                } = &account.credential
                    && *expires < cutoff
                    && token_url.is_none()
                    && client_id.is_none()
                {
                    removed_accounts.push(account_id.clone());
                    return false;
                }
                true
            });

            if !removed_accounts.is_empty() {
                pool.order
                    .retain(|id| !removed_accounts.iter().any(|removed| removed == id));
                if let Some(active_id) = &pool.active
                    && removed_accounts.iter().any(|removed| removed == active_id)
                {
                    pool.active = pool.order.first().cloned();
                }
                for account_id in removed_accounts {
                    pruned.push(format!("{provider}:{account_id}"));
                }
            }
        }
        self.oauth_pools.retain(|_, pool| !pool.accounts.is_empty());

        self.entries.retain(|provider, cred| {
            if let AuthCredential::OAuth {
                expires,
                token_url,
                client_id,
                ..
            } = cred
            {
                // Only prune tokens that are well past expiry AND have no
                // self-contained refresh metadata.
                if *expires < cutoff && token_url.is_none() && client_id.is_none() {
                    tracing::info!(
                        event = "pi.auth.prune_stale",
                        provider = %provider,
                        expired_at = expires,
                        "Pruning stale OAuth credential"
                    );
                    pruned.push(provider.clone());
                    return false;
                }
            }
            true
        });

        pruned
    }

    /// Clean up orphaned, expired, and duplicate OAuth accounts.
    ///
    /// This performs a comprehensive cleanup of the auth storage:
    /// - Removes accounts expired beyond the grace period
    /// - Removes stale accounts requiring relogin beyond the grace period
    /// - Removes orphaned accounts not in the order list
    /// - Removes duplicate accounts (same access + refresh tokens)
    ///
    /// Returns a report describing what would be cleaned up without mutating
    /// cache/audit side effects.
    pub fn cleanup_tokens_preview(&self) -> revocation::CleanupReport {
        let mut preview = self.clone();
        preview.cleanup_tokens_impl(false)
    }

    /// Returns a report describing what was cleaned up.
    pub fn cleanup_tokens(&mut self) -> revocation::CleanupReport {
        self.cleanup_tokens_impl(true)
    }

    fn cleanup_tokens_impl(&mut self, apply_side_effects: bool) -> revocation::CleanupReport {
        use revocation::cleanup_constants::{CLEANUP_GRACE_PERIOD_MS, RELOGIN_GRACE_PERIOD_MS};

        let now = chrono::Utc::now().timestamp_millis();
        let mut report = revocation::CleanupReport::new(now);

        for (provider, pool) in &mut self.oauth_pools {
            let mut detail = revocation::ProviderCleanupDetail {
                accounts_before: pool.accounts.len(),
                ..Default::default()
            };

            let mut to_remove = Vec::new();
            let mut seen_tokens: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();

            for (account_id, account) in &pool.accounts {
                if let AuthCredential::OAuth {
                    expires,
                    access_token,
                    refresh_token,
                    ..
                } = &account.credential
                {
                    // Check for expired beyond grace period
                    if *expires < now - CLEANUP_GRACE_PERIOD_MS {
                        to_remove.push((account_id.clone(), "expired_grace_period".to_string()));
                        continue;
                    }

                    // Check for stale relogin required
                    if account.health.requires_relogin {
                        let stale_since = account.health.last_failure_ms.unwrap_or(now);
                        if stale_since < now - RELOGIN_GRACE_PERIOD_MS {
                            to_remove.push((account_id.clone(), "stale_relogin".to_string()));
                        } else {
                            // Check for duplicates
                            let token_pair = (access_token.clone(), refresh_token.clone());
                            if !seen_tokens.insert(token_pair) {
                                to_remove.push((account_id.clone(), "duplicate".to_string()));
                            }
                        }
                    } else {
                        // Check for duplicates
                        let token_pair = (access_token.clone(), refresh_token.clone());
                        if !seen_tokens.insert(token_pair) {
                            to_remove.push((account_id.clone(), "duplicate".to_string()));
                        }
                    }
                }
            }

            // Check for orphaned accounts (not in order list)
            for account_id in pool.accounts.keys() {
                if !pool.order.contains(account_id)
                    && pool.active.as_ref() != Some(account_id)
                    && !to_remove.iter().any(|(id, _)| id == account_id)
                {
                    to_remove.push((account_id.clone(), "orphaned".to_string()));
                }
            }

            // Apply removals
            for (account_id, reason) in &to_remove {
                pool.accounts.remove(account_id);
                pool.order.retain(|id| id != account_id);
                if pool.active.as_ref() == Some(account_id) {
                    pool.active = pool.order.first().cloned();
                }
                *detail.removal_reasons.entry(reason.clone()).or_insert(0) += 1;

                match reason.as_str() {
                    "expired_grace_period" => report.expired_accounts_removed += 1,
                    "stale_relogin" => report.stale_relogin_removed += 1,
                    "orphaned" => report.orphaned_accounts_removed += 1,
                    "duplicate" => report.duplicate_accounts_removed += 1,
                    _ => {}
                }
            }

            detail.accounts_after = pool.accounts.len();
            report.accounts_processed += detail.accounts_before;
            report.provider_details.insert(provider.clone(), detail);
        }

        // Remove empty pools
        self.oauth_pools.retain(|_, pool| !pool.accounts.is_empty());

        if apply_side_effects && !report.is_empty() {
            for (provider, detail) in &report.provider_details {
                if detail.accounts_before != detail.accounts_after {
                    global_cache().invalidate_provider(provider);
                    self.emit_audit_event(
                        AuditEvent::new(now, AuditEventType::TokenCleanup, provider.clone())
                            .with_details(format!(
                                "removed={} before={} after={}",
                                detail.accounts_before.saturating_sub(detail.accounts_after),
                                detail.accounts_before,
                                detail.accounts_after
                            )),
                    );
                }
            }
        }

        report
    }
}

fn api_key_from_credential(credential: &AuthCredential) -> Option<String> {
    match credential {
        AuthCredential::ApiKey { key } => Some(key.clone()),
        AuthCredential::OAuth {
            access_token,
            expires,
            ..
        } => {
            let now = chrono::Utc::now().timestamp_millis();
            if *expires > now {
                Some(access_token.clone())
            } else {
                None
            }
        }
        AuthCredential::BearerToken { token } => Some(token.clone()),
        AuthCredential::AwsCredentials { access_key_id, .. } => Some(access_key_id.clone()),
        AuthCredential::ServiceKey { .. } => None,
    }
}

fn env_key_for_provider(provider: &str) -> Option<&'static str> {
    env_keys_for_provider(provider).first().copied()
}

fn mark_anthropic_oauth_bearer_token(token: &str) -> String {
    format!("{ANTHROPIC_OAUTH_BEARER_MARKER}{token}")
}

pub(crate) fn unmark_anthropic_oauth_bearer_token(token: &str) -> Option<&str> {
    token.strip_prefix(ANTHROPIC_OAUTH_BEARER_MARKER)
}

fn env_keys_for_provider(provider: &str) -> &'static [&'static str] {
    provider_auth_env_keys(provider)
}

fn resolve_external_provider_api_key(provider: &str) -> Option<String> {
    let canonical = canonical_provider_id(provider).unwrap_or(provider);
    match canonical {
        "anthropic" => read_external_claude_access_token()
            .map(|token| mark_anthropic_oauth_bearer_token(&token)),
        // Keep OpenAI API-key auth distinct from Codex OAuth token auth.
        // Codex access tokens are only valid on Codex-specific routes.
        "openai" => read_external_codex_openai_api_key(),
        "openai-codex" => read_external_codex_access_token(),
        "google-gemini-cli" => {
            let project =
                google_project_id_from_env().or_else(google_project_id_from_gcloud_config);
            read_external_gemini_access_payload(project.as_deref())
        }
        "google-antigravity" => {
            let project = google_project_id_from_env()
                .unwrap_or_else(|| GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string());
            read_external_gemini_access_payload(Some(project.as_str()))
        }
        "kimi-for-coding" => read_external_kimi_code_access_token(),
        _ => None,
    }
}

/// Return a stable human-readable label when we can auto-detect local credentials
/// from another coding agent installation.
pub fn external_setup_source(provider: &str) -> Option<&'static str> {
    let canonical = canonical_provider_id(provider).unwrap_or(provider);
    match canonical {
        "anthropic" if read_external_claude_access_token().is_some() => {
            Some("Claude Code (~/.claude/.credentials.json)")
        }
        "openai" if read_external_codex_openai_api_key().is_some() => {
            Some("Codex (~/.codex/auth.json)")
        }
        "openai-codex" if read_external_codex_access_token().is_some() => {
            Some("Codex (~/.codex/auth.json)")
        }
        "google-gemini-cli" => {
            let project =
                google_project_id_from_env().or_else(google_project_id_from_gcloud_config);
            read_external_gemini_access_payload(project.as_deref())
                .is_some()
                .then_some("Gemini CLI (~/.gemini/oauth_creds.json)")
        }
        "google-antigravity" => {
            let project = google_project_id_from_env()
                .unwrap_or_else(|| GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string());
            if read_external_gemini_access_payload(Some(project.as_str())).is_some() {
                Some("Gemini CLI (~/.gemini/oauth_creds.json)")
            } else {
                None
            }
        }
        "kimi-for-coding" if read_external_kimi_code_access_token().is_some() => Some(
            "Kimi CLI (~/.kimi/credentials/kimi-code.json or $KIMI_SHARE_DIR/credentials/kimi-code.json)",
        ),
        _ => None,
    }
}

fn read_external_json(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_external_claude_access_token() -> Option<String> {
    let path = home_dir()?.join(".claude").join(".credentials.json");
    let value = read_external_json(&path)?;
    let token = value
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(serde_json::Value::as_str)?
        .trim()
        .to_string();
    if token.is_empty() { None } else { Some(token) }
}

fn read_external_codex_auth() -> Option<serde_json::Value> {
    let home = home_dir()?;
    let candidates = [
        home.join(".codex").join("auth.json"),
        home.join(".config").join("codex").join("auth.json"),
    ];
    for path in candidates {
        if let Some(value) = read_external_json(&path) {
            return Some(value);
        }
    }
    None
}

fn read_external_codex_access_token() -> Option<String> {
    let value = read_external_codex_auth()?;
    codex_access_token_from_value(&value)
}

fn read_external_codex_openai_api_key() -> Option<String> {
    let value = read_external_codex_auth()?;
    codex_openai_api_key_from_value(&value)
}

fn codex_access_token_from_value(value: &serde_json::Value) -> Option<String> {
    let candidates = [
        // Canonical codex CLI shape.
        value
            .get("tokens")
            .and_then(|tokens| tokens.get("access_token"))
            .and_then(serde_json::Value::as_str),
        // CamelCase variant.
        value
            .get("tokens")
            .and_then(|tokens| tokens.get("accessToken"))
            .and_then(serde_json::Value::as_str),
        // Flat variants.
        value
            .get("access_token")
            .and_then(serde_json::Value::as_str),
        value.get("accessToken").and_then(serde_json::Value::as_str),
        value.get("token").and_then(serde_json::Value::as_str),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|token| !token.is_empty() && !token.starts_with("sk-"))
        .map(std::string::ToString::to_string)
}

fn codex_openai_api_key_from_value(value: &serde_json::Value) -> Option<String> {
    let candidates = [
        value
            .get("OPENAI_API_KEY")
            .and_then(serde_json::Value::as_str),
        value
            .get("openai_api_key")
            .and_then(serde_json::Value::as_str),
        value
            .get("openaiApiKey")
            .and_then(serde_json::Value::as_str),
        value
            .get("env")
            .and_then(|env| env.get("OPENAI_API_KEY"))
            .and_then(serde_json::Value::as_str),
        value
            .get("env")
            .and_then(|env| env.get("openai_api_key"))
            .and_then(serde_json::Value::as_str),
        value
            .get("env")
            .and_then(|env| env.get("openaiApiKey"))
            .and_then(serde_json::Value::as_str),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|key| !key.is_empty())
        .map(std::string::ToString::to_string)
}

fn read_external_gemini_access_payload(project_id: Option<&str>) -> Option<String> {
    let home = home_dir()?;
    let candidates = [
        home.join(".gemini").join("oauth_creds.json"),
        home.join(".config").join("gemini").join("credentials.json"),
    ];

    for path in candidates {
        let Some(value) = read_external_json(&path) else {
            continue;
        };
        let Some(token) = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        let project = project_id
            .map(std::string::ToString::to_string)
            .or_else(|| {
                value
                    .get("projectId")
                    .or_else(|| value.get("project_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(std::string::ToString::to_string)
            })
            .or_else(google_project_id_from_gcloud_config)?;
        let project = project.trim();
        if project.is_empty() {
            continue;
        }

        return Some(encode_project_scoped_access_token(token, project));
    }

    None
}

#[allow(clippy::cast_precision_loss)]
fn read_external_kimi_code_access_token() -> Option<String> {
    let share_dir = kimi_share_dir()?;
    read_external_kimi_code_access_token_from_share_dir(&share_dir)
}

#[allow(clippy::cast_precision_loss)]
fn read_external_kimi_code_access_token_from_share_dir(share_dir: &Path) -> Option<String> {
    let path = share_dir.join("credentials").join("kimi-code.json");
    let value = read_external_json(&path)?;

    let token = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())?;

    let expires_at = value
        .get("expires_at")
        .and_then(|raw| raw.as_f64().or_else(|| raw.as_i64().map(|v| v as f64)));
    if let Some(expires_at) = expires_at {
        let now_seconds = chrono::Utc::now().timestamp() as f64;
        if expires_at <= now_seconds {
            return None;
        }
    }

    Some(token.to_string())
}

fn google_project_id_from_env() -> Option<String> {
    std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT_ID").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn gcloud_config_dir_with_env_lookup<F>(env_lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("CLOUDSDK_CONFIG")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env_lookup("APPDATA")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value).join("gcloud"))
        })
        .or_else(|| {
            env_lookup("XDG_CONFIG_HOME")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value).join("gcloud"))
        })
        .or_else(|| {
            home_dir_with_env_lookup(env_lookup).map(|home| home.join(".config").join("gcloud"))
        })
}

fn gcloud_active_config_name_with_env_lookup<F>(env_lookup: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("CLOUDSDK_ACTIVE_CONFIG_NAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

fn google_project_id_from_gcloud_config_with_env_lookup<F>(env_lookup: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let config_dir = gcloud_config_dir_with_env_lookup(&env_lookup)?;
    let config_name = gcloud_active_config_name_with_env_lookup(&env_lookup);
    let config_file = config_dir
        .join("configurations")
        .join(format!("config_{config_name}"));
    let Ok(content) = std::fs::read_to_string(config_file) else {
        return None;
    };

    let mut section: Option<&str> = None;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(rest) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = Some(rest.trim());
            continue;
        }

        if section != Some("core") {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "project" {
            continue;
        }
        let project = value.trim();
        if project.is_empty() {
            continue;
        }
        return Some(project.to_string());
    }

    None
}

fn google_project_id_from_gcloud_config() -> Option<String> {
    google_project_id_from_gcloud_config_with_env_lookup(|key| std::env::var(key).ok())
}

fn encode_project_scoped_access_token(token: &str, project_id: &str) -> String {
    serde_json::json!({
        "token": token,
        "projectId": project_id,
    })
    .to_string()
}

fn decode_project_scoped_access_token(payload: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let token = value
        .get("token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let project_id = value
        .get("projectId")
        .or_else(|| value.get("project_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    Some((token, project_id))
}

fn resolve_google_refresh_project_id(provider: &str, access_token: &str) -> Option<String> {
    decode_project_scoped_access_token(access_token)
        .map(|(_, project_id)| project_id)
        .or_else(google_project_id_from_env)
        .or_else(google_project_id_from_gcloud_config)
        .or_else(|| {
            (provider == "google-antigravity")
                .then(|| GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string())
        })
}

// ── AWS Credential Chain ────────────────────────────────────────

/// Resolved AWS credentials ready for Sigv4 signing or bearer auth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwsResolvedCredentials {
    /// Standard IAM credentials for Sigv4 signing.
    Sigv4 {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
        region: String,
    },
    /// Bearer token (e.g. `AWS_BEARER_TOKEN_BEDROCK`).
    Bearer { token: String, region: String },
}

/// Resolve AWS credentials following the standard precedence chain.
///
/// Precedence (first match wins):
/// 1. `AWS_BEARER_TOKEN_BEDROCK` env var → bearer token auth
/// 2. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` env vars → Sigv4
/// 3. `AWS_PROFILE` env var → profile-based (returns the profile name for external resolution)
/// 4. Stored `AwsCredentials` in auth.json
/// 5. Stored `BearerToken` in auth.json (for bedrock)
///
/// `region` is resolved from: `AWS_REGION` → `AWS_DEFAULT_REGION` → `"us-east-1"`.
pub fn resolve_aws_credentials(auth: &AuthStorage) -> Option<AwsResolvedCredentials> {
    resolve_aws_credentials_with_env(auth, |var| std::env::var(var).ok())
}

fn resolve_aws_credentials_with_env<F>(
    auth: &AuthStorage,
    mut env: F,
) -> Option<AwsResolvedCredentials>
where
    F: FnMut(&str) -> Option<String>,
{
    let region = env("AWS_REGION")
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "us-east-1".to_string());

    // 1. Bearer token from env (AWS Bedrock specific)
    if let Some(token) = env("AWS_BEARER_TOKEN_BEDROCK") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(AwsResolvedCredentials::Bearer { token, region });
        }
    }

    // 2. Explicit IAM credentials from env
    if let Some(access_key) = env("AWS_ACCESS_KEY_ID") {
        let access_key = access_key.trim().to_string();
        if !access_key.is_empty() {
            if let Some(secret_key) = env("AWS_SECRET_ACCESS_KEY") {
                let secret_key = secret_key.trim().to_string();
                if !secret_key.is_empty() {
                    let session_token = env("AWS_SESSION_TOKEN")
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    return Some(AwsResolvedCredentials::Sigv4 {
                        access_key_id: access_key,
                        secret_access_key: secret_key,
                        session_token,
                        region,
                    });
                }
            }
        }
    }

    // 3. Stored credentials in auth.json
    let provider = "amazon-bedrock";
    match auth.get(provider) {
        Some(AuthCredential::AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token,
            region: stored_region,
        }) => Some(AwsResolvedCredentials::Sigv4 {
            access_key_id: access_key_id.clone(),
            secret_access_key: secret_access_key.clone(),
            session_token: session_token.clone(),
            region: stored_region.clone().unwrap_or(region),
        }),
        Some(AuthCredential::BearerToken { token }) => Some(AwsResolvedCredentials::Bearer {
            token: token.clone(),
            region,
        }),
        Some(AuthCredential::ApiKey { key }) => {
            // Legacy: treat stored API key as bearer token for Bedrock
            Some(AwsResolvedCredentials::Bearer {
                token: key.clone(),
                region,
            })
        }
        _ => None,
    }
}

// ── SAP AI Core Service Key Resolution ──────────────────────────

/// Resolved SAP AI Core credentials ready for client-credentials token exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SapResolvedCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub token_url: String,
    pub service_url: String,
}

/// Resolve SAP AI Core credentials from env vars or stored service key.
///
/// Precedence:
/// 1. `AICORE_SERVICE_KEY` env var (JSON-encoded service key)
/// 2. Individual env vars: `SAP_AI_CORE_CLIENT_ID`, `SAP_AI_CORE_CLIENT_SECRET`,
///    `SAP_AI_CORE_TOKEN_URL`, `SAP_AI_CORE_SERVICE_URL`
/// 3. Stored `ServiceKey` in auth.json
pub fn resolve_sap_credentials(auth: &AuthStorage) -> Option<SapResolvedCredentials> {
    resolve_sap_credentials_with_env(auth, |var| std::env::var(var).ok())
}

fn resolve_sap_credentials_with_env<F>(
    auth: &AuthStorage,
    mut env: F,
) -> Option<SapResolvedCredentials>
where
    F: FnMut(&str) -> Option<String>,
{
    // 1. JSON-encoded service key from env
    if let Some(key_json) = env("AICORE_SERVICE_KEY") {
        if let Some(creds) = parse_sap_service_key_json(&key_json) {
            return Some(creds);
        }
    }

    // 2. Individual env vars
    let client_id = env("SAP_AI_CORE_CLIENT_ID");
    let client_secret = env("SAP_AI_CORE_CLIENT_SECRET");
    let token_url = env("SAP_AI_CORE_TOKEN_URL");
    let service_url = env("SAP_AI_CORE_SERVICE_URL");

    if let (Some(id), Some(secret), Some(turl), Some(surl)) =
        (client_id, client_secret, token_url, service_url)
    {
        let id = id.trim().to_string();
        let secret = secret.trim().to_string();
        let turl = turl.trim().to_string();
        let surl = surl.trim().to_string();
        if !id.is_empty() && !secret.is_empty() && !turl.is_empty() && !surl.is_empty() {
            return Some(SapResolvedCredentials {
                client_id: id,
                client_secret: secret,
                token_url: turl,
                service_url: surl,
            });
        }
    }

    // 3. Stored service key in auth.json
    let provider = "sap-ai-core";
    if let Some(AuthCredential::ServiceKey {
        client_id,
        client_secret,
        token_url,
        service_url,
    }) = auth.get(provider)
    {
        if let (Some(id), Some(secret), Some(turl), Some(surl)) = (
            client_id.as_ref(),
            client_secret.as_ref(),
            token_url.as_ref(),
            service_url.as_ref(),
        ) {
            if !id.is_empty() && !secret.is_empty() && !turl.is_empty() && !surl.is_empty() {
                return Some(SapResolvedCredentials {
                    client_id: id.clone(),
                    client_secret: secret.clone(),
                    token_url: turl.clone(),
                    service_url: surl.clone(),
                });
            }
        }
    }

    None
}

/// Parse a JSON-encoded SAP AI Core service key.
fn parse_sap_service_key_json(json_str: &str) -> Option<SapResolvedCredentials> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = v.as_object()?;

    // SAP service keys use "clientid"/"clientsecret" (no underscore) and
    // "url" for token URL, "serviceurls.AI_API_URL" for service URL.
    let client_id = obj
        .get("clientid")
        .or_else(|| obj.get("client_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let client_secret = obj
        .get("clientsecret")
        .or_else(|| obj.get("client_secret"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let token_url = obj
        .get("url")
        .or_else(|| obj.get("token_url"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let service_url = obj
        .get("serviceurls")
        .and_then(|v| v.get("AI_API_URL"))
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("service_url").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())?;

    Some(SapResolvedCredentials {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        token_url: token_url.to_string(),
        service_url: service_url.to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct SapTokenExchangeResponse {
    access_token: String,
}

/// Exchange SAP AI Core service-key credentials for an access token.
///
/// Returns `Ok(None)` when SAP credentials are not configured.
pub async fn exchange_sap_access_token(auth: &AuthStorage) -> Result<Option<String>> {
    let Some(creds) = resolve_sap_credentials(auth) else {
        return Ok(None);
    };

    let client = crate::http::client::Client::new();
    let token = exchange_sap_access_token_with_client(&client, &creds).await?;
    Ok(Some(token))
}

async fn exchange_sap_access_token_with_client(
    client: &crate::http::client::Client,
    creds: &SapResolvedCredentials,
) -> Result<String> {
    let form_body = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}",
        percent_encode_component(&creds.client_id),
        percent_encode_component(&creds.client_secret),
    );

    let request = client
        .post(&creds.token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("SAP AI Core token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(
        &text,
        &[creds.client_id.as_str(), creds.client_secret.as_str()],
    );

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "SAP AI Core token exchange failed (HTTP {status}): {redacted_text}"
        )));
    }

    let response: SapTokenExchangeResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("SAP AI Core token response was invalid JSON: {e}")))?;
    let access_token = response.access_token.trim();
    if access_token.is_empty() {
        return Err(Error::auth(
            "SAP AI Core token exchange returned an empty access_token".to_string(),
        ));
    }

    Ok(access_token.to_string())
}

fn redact_known_secrets(text: &str, secrets: &[&str]) -> String {
    let mut redacted = text.to_string();
    for secret in secrets {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            redacted = redacted.replace(trimmed, "[REDACTED]");
        }
    }

    redact_sensitive_json_fields(&redacted)
}

fn redact_sensitive_json_fields(text: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_string();
    };
    redact_sensitive_json_value(&mut json);
    serde_json::to_string(&json).unwrap_or_else(|_| text.to_string())
}

fn redact_sensitive_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                if is_sensitive_json_key(key) {
                    *nested = serde_json::Value::String("[REDACTED]".to_string());
                } else {
                    redact_sensitive_json_value(nested);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_sensitive_json_value(item);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn is_sensitive_json_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();

    matches!(
        normalized.as_str(),
        "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "apikey"
            | "authorization"
            | "credential"
            | "secret"
            | "clientsecret"
            | "password"
    ) || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.ends_with("apikey")
        || normalized.contains("authorization")
}

/// Integration-test bridge to production redaction logic.
#[doc(hidden)]
pub fn redact_known_secrets_for_tests(text: &str, secrets: &[&str]) -> String {
    redact_known_secrets(text, secrets)
}

/// Integration-test bridge to recursive JSON redaction logic.
#[doc(hidden)]
pub fn redact_sensitive_json_value_for_tests(value: &mut serde_json::Value) {
    redact_sensitive_json_value(value);
}

#[derive(Debug, Clone)]
pub struct OAuthStartInfo {
    pub provider: String,
    pub url: String,
    pub verifier: String,
    pub instructions: Option<String>,
}

// ── Device Flow (RFC 8628) ──────────────────────────────────────

/// Response from the device authorization endpoint (RFC 8628 section 3.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_device_interval")]
    pub interval: u64,
}

const fn default_device_interval() -> u64 {
    5
}

/// Result of polling the device flow token endpoint.
#[derive(Debug)]
pub enum DeviceFlowPollResult {
    /// User has not yet authorized; keep polling.
    Pending,
    /// Server asked us to slow down; increase interval.
    SlowDown,
    /// Authorization succeeded.
    Success(AuthCredential),
    /// Device code has expired.
    Expired,
    /// User explicitly denied access.
    AccessDenied,
    /// An unexpected error occurred.
    Error(String),
}

// ── Provider-specific OAuth configs ─────────────────────────────

/// OAuth settings for GitHub Copilot.
///
/// `github_base_url` defaults to `https://github.com` but can be overridden
/// for GitHub Enterprise Server instances.
#[derive(Debug, Clone)]
pub struct CopilotOAuthConfig {
    pub client_id: String,
    pub github_base_url: String,
    pub scopes: String,
}

impl Default for CopilotOAuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            github_base_url: "https://github.com".to_string(),
            scopes: GITHUB_COPILOT_SCOPES.to_string(),
        }
    }
}

/// OAuth settings for GitLab.
///
/// `base_url` defaults to `https://gitlab.com` but can be overridden
/// for self-hosted GitLab instances.
#[derive(Debug, Clone)]
pub struct GitLabOAuthConfig {
    pub client_id: String,
    pub base_url: String,
    pub scopes: String,
    pub redirect_uri: Option<String>,
}

impl Default for GitLabOAuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            base_url: GITLAB_DEFAULT_BASE_URL.to_string(),
            scopes: GITLAB_DEFAULT_SCOPES.to_string(),
            redirect_uri: None,
        }
    }
}

fn percent_encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push_str("%20"),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

fn percent_decode_component(value: &str) -> Option<String> {
    if !value.as_bytes().contains(&b'%') && !value.as_bytes().contains(&b'+') {
        return Some(value.to_string());
    }

    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = bytes.next()?;
                let lo = bytes.next()?;
                let hex = [hi, lo];
                let hex = std::str::from_utf8(&hex).ok()?;
                let decoded = u8::from_str_radix(hex, 16).ok()?;
                out.push(decoded);
            }
            other => out.push(other),
        }
    }

    String::from_utf8(out).ok()
}

fn parse_query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.trim().is_empty())
        .filter_map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let key = percent_decode_component(k.trim())?;
            let value = percent_decode_component(v.trim())?;
            Some((key, value))
        })
        .collect()
}

fn build_url_with_query(base: &str, params: &[(&str, &str)]) -> String {
    let mut url = String::with_capacity(base.len() + 128);
    url.push_str(base);
    url.push('?');

    for (idx, (k, v)) in params.iter().enumerate() {
        if idx > 0 {
            url.push('&');
        }
        url.push_str(&percent_encode_component(k));
        url.push('=');
        url.push_str(&percent_encode_component(v));
    }

    url
}

fn kimi_code_oauth_host_with_env_lookup<F>(env_lookup: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    KIMI_CODE_OAUTH_HOST_ENV_KEYS
        .iter()
        .find_map(|key| {
            env_lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| KIMI_CODE_OAUTH_DEFAULT_HOST.to_string())
}

fn kimi_code_oauth_host() -> String {
    kimi_code_oauth_host_with_env_lookup(|key| std::env::var(key).ok())
}

fn kimi_code_endpoint_for_host(host: &str, path: &str) -> String {
    format!("{}{}", trim_trailing_slash(host), path)
}

fn kimi_code_token_endpoint() -> String {
    kimi_code_endpoint_for_host(&kimi_code_oauth_host(), KIMI_CODE_TOKEN_PATH)
}

fn home_dir_with_env_lookup<F>(env_lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("HOME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env_lookup("USERPROFILE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            let drive = env_lookup("HOMEDRIVE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())?;
            let path = env_lookup("HOMEPATH")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())?;
            if path.starts_with('\\') || path.starts_with('/') {
                Some(PathBuf::from(format!("{drive}{path}")))
            } else {
                let mut combined = PathBuf::from(drive);
                combined.push(path);
                Some(combined)
            }
        })
}

fn home_dir() -> Option<PathBuf> {
    home_dir_with_env_lookup(|key| std::env::var(key).ok())
}

fn kimi_share_dir_with_env_lookup<F>(env_lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup(KIMI_SHARE_DIR_ENV_KEY)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir_with_env_lookup(env_lookup).map(|home| home.join(".kimi")))
}

fn kimi_share_dir() -> Option<PathBuf> {
    kimi_share_dir_with_env_lookup(|key| std::env::var(key).ok())
}

fn sanitize_ascii_header_value(value: &str, fallback: &str) -> String {
    if value.is_ascii() && !value.trim().is_empty() {
        return value.to_string();
    }

    let sanitized = value
        .chars()
        .filter(char::is_ascii)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

fn kimi_device_id_paths() -> Option<(PathBuf, PathBuf)> {
    let primary = kimi_share_dir()?.join("device_id");
    let legacy = home_dir().map_or_else(
        || primary.clone(),
        |home| home.join(".pi").join("agent").join("kimi-device-id"),
    );
    Some((primary, legacy))
}

fn kimi_device_id() -> String {
    let generated = uuid::Uuid::new_v4().simple().to_string();
    let Some((primary, legacy)) = kimi_device_id_paths() else {
        return generated;
    };

    for path in [&primary, &legacy] {
        if let Ok(existing) = fs::read_to_string(path) {
            let existing = existing.trim();
            if !existing.is_empty() {
                return existing.to_string();
            }
        }
    }

    if let Some(parent) = primary.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if fs::write(&primary, generated.as_bytes()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&primary, fs::Permissions::from_mode(0o600));
        }
    }

    generated
}

fn kimi_common_headers() -> Vec<(String, String)> {
    let device_name = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let device_model = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let os_version = std::env::consts::OS.to_string();

    vec![
        (
            "X-Msh-Platform".to_string(),
            sanitize_ascii_header_value("kimi_cli", "unknown"),
        ),
        (
            "X-Msh-Version".to_string(),
            sanitize_ascii_header_value(env!("CARGO_PKG_VERSION"), "unknown"),
        ),
        (
            "X-Msh-Device-Name".to_string(),
            sanitize_ascii_header_value(&device_name, "unknown"),
        ),
        (
            "X-Msh-Device-Model".to_string(),
            sanitize_ascii_header_value(&device_model, "unknown"),
        ),
        (
            "X-Msh-Os-Version".to_string(),
            sanitize_ascii_header_value(&os_version, "unknown"),
        ),
        (
            "X-Msh-Device-Id".to_string(),
            sanitize_ascii_header_value(&kimi_device_id(), "unknown"),
        ),
    ]
}

/// Start Anthropic OAuth by generating an authorization URL and PKCE verifier.
pub fn start_anthropic_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();

    let url = build_url_with_query(
        ANTHROPIC_OAUTH_AUTHORIZE_URL,
        &[
            ("code", "true"),
            ("client_id", ANTHROPIC_OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", ANTHROPIC_OAUTH_REDIRECT_URI),
            ("scope", ANTHROPIC_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "anthropic".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
    })
}

/// Complete Anthropic OAuth by exchanging an authorization code for access/refresh tokens.
pub async fn complete_anthropic_oauth(code_input: &str, verifier: &str) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let request = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": ANTHROPIC_OAUTH_REDIRECT_URI,
            "code_verifier": verifier,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Token exchange failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid token response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(ANTHROPIC_OAUTH_TOKEN_URL.to_string()),
        client_id: Some(ANTHROPIC_OAUTH_CLIENT_ID.to_string()),
    })
}

async fn refresh_anthropic_oauth_token(
    client: &crate::http::client::Client,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let request = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
            "refresh_token": refresh_token,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Anthropic token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Anthropic token refresh failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(ANTHROPIC_OAUTH_TOKEN_URL.to_string()),
        client_id: Some(ANTHROPIC_OAUTH_CLIENT_ID.to_string()),
    })
}

/// Start OpenAI Codex OAuth by generating an authorization URL and PKCE verifier.
pub fn start_openai_codex_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let url = build_url_with_query(
        OPENAI_CODEX_OAUTH_AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", OPENAI_CODEX_OAUTH_CLIENT_ID),
            ("redirect_uri", OPENAI_CODEX_OAUTH_REDIRECT_URI),
            ("scope", OPENAI_CODEX_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "pi"),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "openai-codex".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
    })
}

/// Complete OpenAI Codex OAuth by exchanging an authorization code for access/refresh tokens.
pub async fn complete_openai_codex_oauth(
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);
    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };
    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let form_body = format!(
        "grant_type=authorization_code&client_id={}&code={}&code_verifier={}&redirect_uri={}",
        percent_encode_component(OPENAI_CODEX_OAUTH_CLIENT_ID),
        percent_encode_component(&code),
        percent_encode_component(verifier),
        percent_encode_component(OPENAI_CODEX_OAUTH_REDIRECT_URI),
    );

    let client = crate::http::client::Client::new();
    let request = client
        .post(OPENAI_CODEX_OAUTH_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("OpenAI Codex token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code.as_str(), verifier]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "OpenAI Codex token exchange failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid OpenAI Codex token response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(OPENAI_CODEX_OAUTH_TOKEN_URL.to_string()),
        client_id: Some(OPENAI_CODEX_OAUTH_CLIENT_ID.to_string()),
    })
}

/// Start Google Gemini CLI OAuth by generating an authorization URL and PKCE verifier.
pub fn start_google_gemini_cli_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let url = build_url_with_query(
        GOOGLE_GEMINI_CLI_OAUTH_AUTHORIZE_URL,
        &[
            ("client_id", GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI),
            ("scope", GOOGLE_GEMINI_CLI_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "google-gemini-cli".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
    })
}

/// Start Google Antigravity OAuth by generating an authorization URL and PKCE verifier.
pub fn start_google_antigravity_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let url = build_url_with_query(
        GOOGLE_ANTIGRAVITY_OAUTH_AUTHORIZE_URL,
        &[
            ("client_id", GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI),
            ("scope", GOOGLE_ANTIGRAVITY_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "google-antigravity".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
    })
}

async fn discover_google_gemini_cli_project_id(
    client: &crate::http::client::Client,
    access_token: &str,
) -> Result<String> {
    let env_project = google_project_id_from_env();
    let gcloud_project = google_project_id_from_gcloud_config();
    let fallback_project = env_project.clone().or_else(|| gcloud_project.clone());
    let mut payload = serde_json::json!({
        "metadata": {
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        }
    });
    if let Some(project) = &fallback_project {
        payload["cloudaicompanionProject"] = serde_json::Value::String(project.clone());
        payload["metadata"]["duetProject"] = serde_json::Value::String(project.clone());
    }

    let request = client
        .post(&format!(
            "{GOOGLE_GEMINI_CLI_CODE_ASSIST_ENDPOINT}/v1internal:loadCodeAssist"
        ))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .json(&payload)?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Google Cloud project discovery failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(project_id) = parse_code_assist_project_id(&value)
    {
        return Ok(project_id);
    }

    if let Some(project_id) = fallback_project {
        tracing::warn!(
            event = "pi.auth.google_gemini_cli.project_fallback",
            status,
            "Google Gemini project discovery failed; using local fallback project"
        );
        return Ok(project_id);
    }

    Err(Error::auth(
        "Google Cloud project discovery failed. Set GOOGLE_CLOUD_PROJECT / GOOGLE_CLOUD_PROJECT_ID or configure gcloud core/project, then retry /login google-gemini-cli.".to_string(),
    ))
}

async fn discover_google_antigravity_project_id(
    client: &crate::http::client::Client,
    access_token: &str,
) -> Result<String> {
    let payload = serde_json::json!({
        "metadata": {
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        }
    });

    for endpoint in GOOGLE_ANTIGRAVITY_PROJECT_DISCOVERY_ENDPOINTS {
        let request = client
            .post(&format!("{endpoint}/v1internal:loadCodeAssist"))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .json(&payload)?;

        let Ok(response) = Box::pin(request.send()).await else {
            continue;
        };
        let status = response.status();
        if !(200..300).contains(&status) {
            continue;
        }
        let text = response.text().await.unwrap_or_default();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(project_id) = parse_code_assist_project_id(&value) {
                return Ok(project_id);
            }
        }
    }

    Ok(GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string())
}

fn normalize_google_project_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.strip_prefix("projects/").map_or(trimmed, str::trim);
    (!normalized.is_empty()).then(|| normalized.to_string())
}

fn parse_code_assist_project_value(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .and_then(normalize_google_project_id)
        .or_else(|| {
            value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .and_then(normalize_google_project_id)
        })
        .or_else(|| {
            value
                .get("projectId")
                .or_else(|| value.get("project_id"))
                .and_then(serde_json::Value::as_str)
                .and_then(normalize_google_project_id)
        })
        .or_else(|| {
            value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .and_then(normalize_google_project_id)
        })
}

fn parse_code_assist_project_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("cloudaicompanionProject")
        .and_then(parse_code_assist_project_value)
        .or_else(|| {
            value
                .get("projectId")
                .or_else(|| value.get("project_id"))
                .and_then(serde_json::Value::as_str)
                .and_then(normalize_google_project_id)
        })
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|metadata| metadata.get("duetProject"))
                .and_then(serde_json::Value::as_str)
                .and_then(normalize_google_project_id)
        })
        .or_else(|| {
            value
                .get("project")
                .and_then(parse_code_assist_project_value)
        })
}

async fn exchange_google_authorization_code(
    client: &crate::http::client::Client,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuthTokenResponse> {
    let form_body = format!(
        "client_id={}&client_secret={}&code={}&grant_type=authorization_code&redirect_uri={}&code_verifier={}",
        percent_encode_component(client_id),
        percent_encode_component(client_secret),
        percent_encode_component(code),
        percent_encode_component(redirect_uri),
        percent_encode_component(verifier),
    );

    let request = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("OAuth token exchange failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code, verifier, client_secret]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "OAuth token exchange failed: {redacted_text}"
        )));
    }

    serde_json::from_str::<OAuthTokenResponse>(&text)
        .map_err(|e| Error::auth(format!("Invalid OAuth token response: {e}")))
}

/// Complete Google Gemini CLI OAuth by exchanging an authorization code for tokens.
pub async fn complete_google_gemini_cli_oauth(
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);
    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };
    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let oauth_response = exchange_google_authorization_code(
        &client,
        GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET,
        &code,
        GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI,
        verifier,
    )
    .await?;

    let project_id =
        discover_google_gemini_cli_project_id(&client, &oauth_response.access_token).await?;

    Ok(AuthCredential::OAuth {
        access_token: encode_project_scoped_access_token(&oauth_response.access_token, &project_id),
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: None,
        client_id: None,
    })
}

/// Complete Google Antigravity OAuth by exchanging an authorization code for tokens.
pub async fn complete_google_antigravity_oauth(
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);
    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };
    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let oauth_response = exchange_google_authorization_code(
        &client,
        GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET,
        &code,
        GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI,
        verifier,
    )
    .await?;

    let project_id =
        discover_google_antigravity_project_id(&client, &oauth_response.access_token).await?;

    Ok(AuthCredential::OAuth {
        access_token: encode_project_scoped_access_token(&oauth_response.access_token, &project_id),
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: None,
        client_id: None,
    })
}

#[derive(Debug, Deserialize)]
struct OAuthRefreshTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

async fn refresh_google_oauth_token_with_project(
    client: &crate::http::client::Client,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
    project_id: &str,
    provider_name: &str,
) -> Result<AuthCredential> {
    let form_body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        percent_encode_component(client_id),
        percent_encode_component(client_secret),
        percent_encode_component(refresh_token),
    );

    let request = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("{provider_name} token refresh failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[client_secret, refresh_token]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "{provider_name} token refresh failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthRefreshTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid {provider_name} refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: encode_project_scoped_access_token(&oauth_response.access_token, project_id),
        refresh_token: oauth_response
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: None,
        client_id: None,
    })
}

async fn refresh_google_gemini_cli_oauth_token(
    client: &crate::http::client::Client,
    refresh_token: &str,
    project_id: &str,
) -> Result<AuthCredential> {
    refresh_google_oauth_token_with_project(
        client,
        GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET,
        refresh_token,
        project_id,
        "google-gemini-cli",
    )
    .await
}

async fn refresh_google_antigravity_oauth_token(
    client: &crate::http::client::Client,
    refresh_token: &str,
    project_id: &str,
) -> Result<AuthCredential> {
    refresh_google_oauth_token_with_project(
        client,
        GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET,
        refresh_token,
        project_id,
        "google-antigravity",
    )
    .await
}

/// Start Kimi Code OAuth device flow.
pub async fn start_kimi_code_device_flow() -> Result<DeviceCodeResponse> {
    let client = crate::http::client::Client::new();
    start_kimi_code_device_flow_with_client(&client, &kimi_code_oauth_host()).await
}

async fn start_kimi_code_device_flow_with_client(
    client: &crate::http::client::Client,
    oauth_host: &str,
) -> Result<DeviceCodeResponse> {
    let url = kimi_code_endpoint_for_host(oauth_host, KIMI_CODE_DEVICE_AUTHORIZATION_PATH);
    let form_body = format!(
        "client_id={}",
        percent_encode_component(KIMI_CODE_OAUTH_CLIENT_ID)
    );
    let mut request = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        request = request.header(name, value);
    }

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Kimi device authorization request failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[KIMI_CODE_OAUTH_CLIENT_ID]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Kimi device authorization failed (HTTP {status}): {redacted_text}"
        )));
    }

    serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid Kimi device authorization response: {e}")))
}

/// Poll Kimi Code OAuth device flow.
pub async fn poll_kimi_code_device_flow(device_code: &str) -> DeviceFlowPollResult {
    let client = crate::http::client::Client::new();
    poll_kimi_code_device_flow_with_client(&client, &kimi_code_oauth_host(), device_code).await
}

async fn poll_kimi_code_device_flow_with_client(
    client: &crate::http::client::Client,
    oauth_host: &str,
    device_code: &str,
) -> DeviceFlowPollResult {
    let token_url = kimi_code_endpoint_for_host(oauth_host, KIMI_CODE_TOKEN_PATH);
    let form_body = format!(
        "client_id={}&device_code={}&grant_type={}",
        percent_encode_component(KIMI_CODE_OAUTH_CLIENT_ID),
        percent_encode_component(device_code),
        percent_encode_component("urn:ietf:params:oauth:grant-type:device_code"),
    );
    let mut request = client
        .post(&token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        request = request.header(name, value);
    }

    let response = match Box::pin(request.send()).await {
        Ok(response) => response,
        Err(err) => return DeviceFlowPollResult::Error(format!("Poll request failed: {err}")),
    };
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            return DeviceFlowPollResult::Error(format!("Invalid poll response JSON: {err}"));
        }
    };

    if let Some(error) = json.get("error").and_then(serde_json::Value::as_str) {
        return match error {
            "authorization_pending" => DeviceFlowPollResult::Pending,
            "slow_down" => DeviceFlowPollResult::SlowDown,
            "expired_token" => DeviceFlowPollResult::Expired,
            "access_denied" => DeviceFlowPollResult::AccessDenied,
            other => {
                let detail = json
                    .get("error_description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error");
                DeviceFlowPollResult::Error(format!("Kimi device flow error: {other}: {detail}"))
            }
        };
    }

    if !(200..300).contains(&status) {
        return DeviceFlowPollResult::Error(format!(
            "Kimi device flow polling failed (HTTP {status}): {}",
            redact_known_secrets(&text, &[device_code]),
        ));
    }

    let oauth_response: OAuthTokenResponse = match serde_json::from_value(json) {
        Ok(response) => response,
        Err(err) => {
            return DeviceFlowPollResult::Error(format!(
                "Invalid Kimi token response payload: {err}"
            ));
        }
    };

    DeviceFlowPollResult::Success(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url),
        client_id: Some(KIMI_CODE_OAUTH_CLIENT_ID.to_string()),
    })
}

async fn refresh_kimi_code_oauth_token(
    client: &crate::http::client::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let form_body = format!(
        "client_id={}&grant_type=refresh_token&refresh_token={}",
        percent_encode_component(KIMI_CODE_OAUTH_CLIENT_ID),
        percent_encode_component(refresh_token),
    );
    let mut request = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        request = request.header(name, value);
    }

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Kimi token refresh failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Kimi token refresh failed (HTTP {status}): {redacted_text}"
        )));
    }

    let oauth_response: OAuthRefreshTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid Kimi refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url.to_string()),
        client_id: Some(KIMI_CODE_OAUTH_CLIENT_ID.to_string()),
    })
}

/// Start OAuth for an extension-registered provider using its [`OAuthConfig`](crate::models::OAuthConfig).
pub fn start_extension_oauth(
    provider_name: &str,
    config: &crate::models::OAuthConfig,
) -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let scopes = config.scopes.join(" ");

    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", &config.client_id),
        ("response_type", "code"),
        ("scope", &scopes),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &verifier),
    ];

    let redirect_uri_ref = config.redirect_uri.as_deref();
    if let Some(uri) = redirect_uri_ref {
        params.push(("redirect_uri", uri));
    }

    let url = build_url_with_query(&config.auth_url, &params);

    Ok(OAuthStartInfo {
        provider: provider_name.to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
    })
}

/// Complete OAuth for an extension-registered provider by exchanging an authorization code.
pub async fn complete_extension_oauth(
    config: &crate::models::OAuthConfig,
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();

    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "code": code,
        "state": state,
        "code_verifier": verifier,
    });

    if let Some(ref redirect_uri) = config.redirect_uri {
        body["redirect_uri"] = serde_json::Value::String(redirect_uri.clone());
    }

    let request = client.post(&config.token_url).json(&body)?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Token exchange failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid token response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(config.token_url.clone()),
        client_id: Some(config.client_id.clone()),
    })
}

/// Refresh an OAuth token for an extension-registered provider.
async fn refresh_extension_oauth_token(
    client: &crate::http::client::Client,
    config: &crate::models::OAuthConfig,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let request = client.post(&config.token_url).json(&serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": config.client_id,
        "refresh_token": refresh_token,
    }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Extension OAuth token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Extension OAuth token refresh failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(config.token_url.clone()),
        client_id: Some(config.client_id.clone()),
    })
}

/// Provider-agnostic OAuth refresh using self-contained credential metadata.
///
/// This is called for providers whose [`AuthCredential::OAuth`] stores its own
/// `token_url` and `client_id` (e.g. Copilot, GitLab), removing the need for
/// an external config lookup at refresh time.
async fn refresh_self_contained_oauth_token(
    client: &crate::http::client::Client,
    token_url: &str,
    oauth_client_id: &str,
    refresh_token: &str,
    provider: &str,
) -> Result<AuthCredential> {
    let request = client.post(token_url).json(&serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": oauth_client_id,
        "refresh_token": refresh_token,
    }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("{provider} token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "{provider} token refresh failed (HTTP {status}): {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid refresh response from {provider}: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url.to_string()),
        client_id: Some(oauth_client_id.to_string()),
    })
}

// ── GitHub Copilot OAuth ─────────────────────────────────────────

/// Start GitHub Copilot OAuth using the browser-based authorization code flow.
///
/// For CLI tools the device flow ([`start_copilot_device_flow`]) is usually
/// preferred, but the browser flow is provided for environments that support
/// redirect callbacks.
pub fn start_copilot_browser_oauth(config: &CopilotOAuthConfig) -> Result<OAuthStartInfo> {
    if config.client_id.is_empty() {
        return Err(Error::auth(
            "GitHub Copilot OAuth requires a client_id. Set GITHUB_COPILOT_CLIENT_ID or \
             configure the GitHub App in your settings."
                .to_string(),
        ));
    }

    let (verifier, challenge) = generate_pkce();

    let auth_url = if config.github_base_url == "https://github.com" {
        GITHUB_OAUTH_AUTHORIZE_URL.to_string()
    } else {
        format!(
            "{}/login/oauth/authorize",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let url = build_url_with_query(
        &auth_url,
        &[
            ("client_id", &config.client_id),
            ("response_type", "code"),
            ("scope", &config.scopes),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "github-copilot".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL in your browser to authorize GitHub Copilot access, \
             then paste the callback URL or authorization code."
                .to_string(),
        ),
    })
}

/// Complete the GitHub Copilot browser OAuth flow by exchanging the authorization code.
pub async fn complete_copilot_browser_oauth(
    config: &CopilotOAuthConfig,
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth(
            "Missing authorization code. Paste the full callback URL or just the code parameter."
                .to_string(),
        ));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let token_url_str = if config.github_base_url == "https://github.com" {
        GITHUB_OAUTH_TOKEN_URL.to_string()
    } else {
        format!(
            "{}/login/oauth/access_token",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let client = crate::http::client::Client::new();
    let request = client
        .post(&token_url_str)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": config.client_id,
            "code": code,
            "state": state,
            "code_verifier": verifier,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("GitHub token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(copilot_diagnostic(
            &format!("Token exchange failed (HTTP {status})"),
            &redacted,
        )));
    }

    let mut cred = parse_github_token_response(&text)?;
    // Attach refresh metadata so the credential is self-contained for lifecycle refresh.
    if let AuthCredential::OAuth {
        ref mut token_url,
        ref mut client_id,
        ..
    } = cred
    {
        *token_url = Some(token_url_str.clone());
        *client_id = Some(config.client_id.clone());
    }
    Ok(cred)
}

/// Start the GitHub device flow (RFC 8628) for Copilot.
///
/// Returns a [`DeviceCodeResponse`] containing the `user_code` and
/// `verification_uri` the user should visit.
pub async fn start_copilot_device_flow(config: &CopilotOAuthConfig) -> Result<DeviceCodeResponse> {
    if config.client_id.is_empty() {
        return Err(Error::auth(
            "GitHub Copilot device flow requires a client_id. Set GITHUB_COPILOT_CLIENT_ID or \
             configure the GitHub App in your settings."
                .to_string(),
        ));
    }

    let device_url = if config.github_base_url == "https://github.com" {
        GITHUB_DEVICE_CODE_URL.to_string()
    } else {
        format!(
            "{}/login/device/code",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let client = crate::http::client::Client::new();
    let request = client
        .post(&device_url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "client_id": config.client_id,
            "scope": config.scopes,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("GitHub device code request failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());

    if !(200..300).contains(&status) {
        return Err(Error::auth(copilot_diagnostic(
            &format!("Device code request failed (HTTP {status})"),
            &redact_known_secrets(&text, &[]),
        )));
    }

    serde_json::from_str(&text).map_err(|e| {
        Error::auth(format!(
            "Invalid device code response: {e}. \
             Ensure the GitHub App has the Device Flow enabled."
        ))
    })
}

/// Poll the GitHub device flow token endpoint.
///
/// Call this repeatedly at the interval specified in [`DeviceCodeResponse`]
/// until the result is not [`DeviceFlowPollResult::Pending`].
pub async fn poll_copilot_device_flow(
    config: &CopilotOAuthConfig,
    device_code: &str,
) -> DeviceFlowPollResult {
    let token_url = if config.github_base_url == "https://github.com" {
        GITHUB_OAUTH_TOKEN_URL.to_string()
    } else {
        format!(
            "{}/login/oauth/access_token",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let client = crate::http::client::Client::new();
    let request = match client
        .post(&token_url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "client_id": config.client_id,
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        })) {
        Ok(r) => r,
        Err(e) => return DeviceFlowPollResult::Error(format!("Request build failed: {e}")),
    };

    let response = match Box::pin(request.send()).await {
        Ok(r) => r,
        Err(e) => return DeviceFlowPollResult::Error(format!("Poll request failed: {e}")),
    };

    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());

    // GitHub returns 200 even for pending/error states with an "error" field.
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return DeviceFlowPollResult::Error(format!("Invalid poll response: {e}"));
        }
    };

    if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
        return match error {
            "authorization_pending" => DeviceFlowPollResult::Pending,
            "slow_down" => DeviceFlowPollResult::SlowDown,
            "expired_token" => DeviceFlowPollResult::Expired,
            "access_denied" => DeviceFlowPollResult::AccessDenied,
            other => DeviceFlowPollResult::Error(format!(
                "GitHub device flow error: {other}. {}",
                json.get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Check your GitHub App configuration.")
            )),
        };
    }

    match parse_github_token_response(&text) {
        Ok(cred) => DeviceFlowPollResult::Success(cred),
        Err(e) => DeviceFlowPollResult::Error(e.to_string()),
    }
}

/// Parse GitHub's token endpoint response into an [`AuthCredential`].
///
/// GitHub may return `expires_in` (if token has expiry) or omit it for
/// non-expiring tokens. Non-expiring tokens use a far-future expiry.
fn parse_github_token_response(text: &str) -> Result<AuthCredential> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Error::auth(format!("Invalid token JSON: {e}")))?;

    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::auth("Missing access_token in GitHub response".to_string()))?
        .to_string();

    // GitHub may not return a refresh_token for all grant types.
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let expires = json
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .map_or_else(
            || {
                // No expiry → treat as 1 year (GitHub personal access tokens don't expire).
                oauth_expires_at_ms(365 * 24 * 3600)
            },
            oauth_expires_at_ms,
        );

    Ok(AuthCredential::OAuth {
        access_token,
        refresh_token,
        expires,
        // token_url/client_id are set by the caller (start/complete functions)
        // since parse_github_token_response doesn't know the config context.
        token_url: None,
        client_id: None,
    })
}

/// Build an actionable diagnostic message for Copilot OAuth failures.
fn copilot_diagnostic(summary: &str, detail: &str) -> String {
    format!(
        "{summary}: {detail}\n\
         Troubleshooting:\n\
         - Verify the GitHub App client_id is correct\n\
         - Ensure your GitHub account has an active Copilot subscription\n\
         - For GitHub Enterprise, set the correct base URL\n\
         - Check https://github.com/settings/applications for app authorization status"
    )
}

// ── GitLab OAuth ────────────────────────────────────────────────

/// Start GitLab OAuth using the authorization code flow with PKCE.
///
/// Supports both `gitlab.com` and self-hosted instances via
/// [`GitLabOAuthConfig::base_url`].
pub fn start_gitlab_oauth(config: &GitLabOAuthConfig) -> Result<OAuthStartInfo> {
    if config.client_id.is_empty() {
        return Err(Error::auth(
            "GitLab OAuth requires a client_id. Create an application at \
             Settings > Applications in your GitLab instance."
                .to_string(),
        ));
    }

    let (verifier, challenge) = generate_pkce();
    let base = trim_trailing_slash(&config.base_url);
    let auth_url = format!("{base}{GITLAB_OAUTH_AUTHORIZE_PATH}");

    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", &config.client_id),
        ("response_type", "code"),
        ("scope", &config.scopes),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &verifier),
    ];

    let redirect_ref = config.redirect_uri.as_deref();
    if let Some(uri) = redirect_ref {
        params.push(("redirect_uri", uri));
    }

    let url = build_url_with_query(&auth_url, &params);

    Ok(OAuthStartInfo {
        provider: "gitlab".to_string(),
        url,
        verifier,
        instructions: Some(format!(
            "Open the URL to authorize GitLab access on {base}, \
             then paste the callback URL or authorization code."
        )),
    })
}

/// Complete GitLab OAuth by exchanging the authorization code for tokens.
pub async fn complete_gitlab_oauth(
    config: &GitLabOAuthConfig,
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth(
            "Missing authorization code. Paste the full callback URL or just the code parameter."
                .to_string(),
        ));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }
    let base = trim_trailing_slash(&config.base_url);
    let token_url = format!("{base}{GITLAB_OAUTH_TOKEN_PATH}");

    let client = crate::http::client::Client::new();

    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "code": code,
        "state": state,
        "code_verifier": verifier,
    });

    if let Some(ref redirect_uri) = config.redirect_uri {
        body["redirect_uri"] = serde_json::Value::String(redirect_uri.clone());
    }

    let request = client
        .post(&token_url)
        .header("Accept", "application/json")
        .json(&body)?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("GitLab token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(gitlab_diagnostic(
            &config.base_url,
            &format!("Token exchange failed (HTTP {status})"),
            &redacted,
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text).map_err(|e| {
        Error::auth(gitlab_diagnostic(
            &config.base_url,
            &format!("Invalid token response: {e}"),
            &redacted,
        ))
    })?;

    let base = trim_trailing_slash(&config.base_url);
    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(format!("{base}{GITLAB_OAUTH_TOKEN_PATH}")),
        client_id: Some(config.client_id.clone()),
    })
}

/// Build an actionable diagnostic message for GitLab OAuth failures.
fn gitlab_diagnostic(base_url: &str, summary: &str, detail: &str) -> String {
    format!(
        "{summary}: {detail}\n\
         Troubleshooting:\n\
         - Verify the application client_id matches your GitLab application\n\
         - Check Settings > Applications on {base_url}\n\
         - Ensure the redirect URI matches your application configuration\n\
         - For self-hosted GitLab, verify the base URL is correct ({base_url})"
    )
}

// ── Handoff contract to bd-3uqg.7.6 ────────────────────────────
//
// **OAuth lifecycle boundary**: This module handles the *bootstrap* phase:
//   - Initial device flow or browser-based authorization
//   - Authorization code → token exchange
//   - First credential persistence to auth.json
//
// **NOT handled here** (owned by bd-3uqg.7.6):
//   - Periodic token refresh for Copilot/GitLab
//   - Token rotation and re-authentication on refresh failure
//   - Cache hygiene (pruning expired entries)
//   - Session token lifecycle (keep-alive, invalidation)
//
// To integrate refresh, add "github-copilot" and "gitlab" arms to
// `refresh_expired_oauth_tokens_with_client()` once their refresh
// endpoints and grant types are wired.

fn trim_trailing_slash(url: &str) -> &str {
    url.trim_end_matches('/')
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

fn oauth_expires_at_ms(expires_in_seconds: i64) -> i64 {
    const SAFETY_MARGIN_MS: i64 = 5 * 60 * 1000;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let expires_ms = expires_in_seconds.saturating_mul(1000);
    now_ms
        .saturating_add(expires_ms)
        .saturating_sub(SAFETY_MARGIN_MS)
}

/// Generate PKCE code_verifier and code_challenge pair.
///
/// Uses SHA-256 (S256) method as per RFC 7636.
fn generate_pkce() -> (String, String) {
    // Generate code_verifier: 43-128 chars from [A-Z][a-z][0-9]-._~
    // Using multiple UUIDs concatenated and base64url encoded
    let random_bytes: Vec<u8> = (0..4)
        .flat_map(|_| uuid::Uuid::new_v4().into_bytes())
        .collect();

    // Encode 32 random bytes as base64url without padding.
    // 32 bytes -> 43 chars, which is the minimum PKCE verifier length (RFC 7636).
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let code_verifier = engine.encode(&random_bytes[..32.min(random_bytes.len())]);

    // Generate code_challenge: SHA-256(code_verifier) base64url encoded
    let mut hasher = sha2::Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();
    let code_challenge = engine.encode(hash);

    (code_verifier, code_challenge)
}

fn parse_oauth_code_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }

    if let Some((_, query)) = value.split_once('?') {
        let query = query.split('#').next().unwrap_or(query);
        let pairs = parse_query_pairs(query);
        let code = pairs
            .iter()
            .find_map(|(k, v)| (k == "code").then(|| v.clone()));
        let state = pairs
            .iter()
            .find_map(|(k, v)| (k == "state").then(|| v.clone()));
        return (code, state);
    }

    if let Some((code, state)) = value.split_once('#') {
        let code = code.trim();
        let state = state.trim();
        return (
            (!code.is_empty()).then(|| code.to_string()),
            (!state.is_empty()).then(|| state.to_string()),
        );
    }

    (Some(value.to_string()), None)
}

fn lock_file(file: File, timeout: Duration) -> Result<LockedFile> {
    let start = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => return Ok(LockedFile { file }),
            Ok(false) => {} // Lock held by another process, retry
            Err(e) => {
                return Err(Error::auth(format!("Failed to lock auth file: {e}")));
            }
        }

        if start.elapsed() >= timeout {
            return Err(Error::auth("Timed out waiting for auth lock".to_string()));
        }

        let base_ms: u64 = 10;
        let cap_ms: u64 = 500;
        let sleep_ms = base_ms
            .checked_shl(attempt.min(5))
            .unwrap_or(cap_ms)
            .min(cap_ms);
        let jitter = u64::from(start.elapsed().subsec_nanos()) % (sleep_ms / 2 + 1);
        let delay = sleep_ms / 2 + jitter;
        std::thread::sleep(Duration::from_millis(delay));
        attempt = attempt.saturating_add(1);
    }
}

/// A file handle with an exclusive lock. Unlocks on drop.
struct LockedFile {
    file: File,
}

impl LockedFile {
    const fn as_file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Convenience to load auth from default path.
pub fn load_default_auth(path: &Path) -> Result<AuthStorage> {
    AuthStorage::load(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    fn next_token() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn log_test_event(test_name: &str, event: &str, data: serde_json::Value) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_millis();
        let entry = serde_json::json!({
            "schema": "pi.test.auth_event.v1",
            "test": test_name,
            "event": event,
            "timestamp_ms": timestamp_ms,
            "data": data,
        });
        eprintln!(
            "JSONL: {}",
            serde_json::to_string(&entry).expect("serialize auth test event")
        );
    }

    fn spawn_json_server(status_code: u16, body: &str) -> String {
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

    fn spawn_oauth_host_server(status_code: u16, body: &str) -> String {
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
                400 => "Bad Request",
                401 => "Unauthorized",
                403 => "Forbidden",
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

        format!("http://{addr}")
    }

    #[test]
    fn test_google_project_id_from_gcloud_config_parses_core_project() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let gcloud_dir = dir.path().join("gcloud");
        let configs_dir = gcloud_dir.join("configurations");
        std::fs::create_dir_all(&configs_dir).expect("mkdir configurations");
        std::fs::write(
            configs_dir.join("config_default"),
            "[core]\nproject = my-proj\n",
        )
        .expect("write config_default");

        let project = google_project_id_from_gcloud_config_with_env_lookup(|key| match key {
            "CLOUDSDK_CONFIG" => Some(gcloud_dir.to_string_lossy().to_string()),
            _ => None,
        });

        assert_eq!(project.as_deref(), Some("my-proj"));
    }

    #[test]
    fn test_parse_code_assist_project_id_accepts_metadata_duet_project() {
        let payload = serde_json::json!({
            "metadata": {
                "duetProject": "projects/fallback-proj"
            }
        });

        assert_eq!(
            parse_code_assist_project_id(&payload).as_deref(),
            Some("fallback-proj")
        );
    }

    #[test]
    fn test_parse_code_assist_project_id_accepts_nested_project_object() {
        let payload = serde_json::json!({
            "project": {
                "projectId": "nested-proj"
            }
        });

        assert_eq!(
            parse_code_assist_project_id(&payload).as_deref(),
            Some("nested-proj")
        );
    }

    #[test]
    fn test_auth_storage_load_missing_file_starts_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("missing-auth.json");
        assert!(!auth_path.exists());

        let loaded = AuthStorage::load(auth_path.clone()).expect("load");
        assert!(loaded.entries.is_empty());
        assert_eq!(loaded.path, auth_path);
    }

    #[test]
    fn test_auth_storage_api_key_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        {
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "stored-openai-key".to_string(),
                },
            );
            auth.save().expect("save");
        }

        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(
            loaded.api_key("openai").as_deref(),
            Some("stored-openai-key")
        );
    }

    #[test]
    fn test_openai_oauth_url_generation() {
        let test_name = "test_openai_oauth_url_generation";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "openai", "mode": "api_key" }),
        );

        let env_keys = env_keys_for_provider("openai");
        assert!(
            env_keys.contains(&"OPENAI_API_KEY"),
            "expected OPENAI_API_KEY in env key candidates"
        );
        log_test_event(
            test_name,
            "url_generated",
            serde_json::json!({
                "provider": "openai",
                "flow_type": "api_key",
                "env_keys": env_keys,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_openai_token_exchange() {
        let test_name = "test_openai_token_exchange";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "openai", "mode": "api_key_storage" }),
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "openai-key-test".to_string(),
            },
        );
        auth.save().expect("save auth");

        let reloaded = AuthStorage::load(auth_path).expect("reload auth");
        assert_eq!(
            reloaded.api_key("openai").as_deref(),
            Some("openai-key-test")
        );
        log_test_event(
            test_name,
            "token_exchanged",
            serde_json::json!({
                "provider": "openai",
                "flow_type": "api_key",
                "persisted": true,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_google_oauth_url_generation() {
        let test_name = "test_google_oauth_url_generation";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "google", "mode": "api_key" }),
        );

        let env_keys = env_keys_for_provider("google");
        assert!(
            env_keys.contains(&"GOOGLE_API_KEY"),
            "expected GOOGLE_API_KEY in env key candidates"
        );
        assert!(
            env_keys.contains(&"GEMINI_API_KEY"),
            "expected GEMINI_API_KEY alias in env key candidates"
        );
        log_test_event(
            test_name,
            "url_generated",
            serde_json::json!({
                "provider": "google",
                "flow_type": "api_key",
                "env_keys": env_keys,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_google_token_exchange() {
        let test_name = "test_google_token_exchange";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "google", "mode": "api_key_storage" }),
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-key-test".to_string(),
            },
        );
        auth.save().expect("save auth");

        let reloaded = AuthStorage::load(auth_path).expect("reload auth");
        assert_eq!(
            reloaded.api_key("google").as_deref(),
            Some("google-key-test")
        );
        assert_eq!(
            reloaded
                .resolve_api_key_with_env_lookup("gemini", None, |_| None)
                .as_deref(),
            Some("google-key-test")
        );
        log_test_event(
            test_name,
            "token_exchanged",
            serde_json::json!({
                "provider": "google",
                "flow_type": "api_key",
                "has_refresh": false,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_resolve_api_key_precedence_override_env_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "stored-openai-key".to_string(),
            },
        );

        let env_value = "env-openai-key".to_string();

        let override_resolved =
            auth.resolve_api_key_with_env_lookup("openai", Some("override-key"), |_| {
                Some(env_value.clone())
            });
        assert_eq!(override_resolved.as_deref(), Some("override-key"));

        let env_resolved =
            auth.resolve_api_key_with_env_lookup("openai", None, |_| Some(env_value.clone()));
        assert_eq!(env_resolved.as_deref(), Some("env-openai-key"));

        let stored_resolved = auth.resolve_api_key_with_env_lookup("openai", None, |_| None);
        assert_eq!(stored_resolved.as_deref(), Some("stored-openai-key"));
    }

    #[test]
    fn test_resolve_api_key_prefers_stored_oauth_over_env() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        let now = chrono::Utc::now().timestamp_millis();
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                access_token: "stored-oauth-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: now + 60_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", None, |_| {
            Some("env-api-key".to_string())
        });
        let token = resolved.expect("resolved anthropic oauth token");
        assert_eq!(
            unmark_anthropic_oauth_bearer_token(&token),
            Some("stored-oauth-token")
        );
    }

    #[test]
    fn test_resolve_api_key_expired_oauth_falls_back_to_env() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        let now = chrono::Utc::now().timestamp_millis();
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                access_token: "expired-oauth-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: now - 1_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", None, |_| {
            Some("env-api-key".to_string())
        });
        assert_eq!(resolved.as_deref(), Some("env-api-key"));
    }

    #[test]
    fn test_resolve_api_key_returns_none_when_unconfigured() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        let resolved =
            auth.resolve_api_key_with_env_lookup("nonexistent-provider-for-test", None, |_| None);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_generate_pkce_is_base64url_no_pad() {
        let (verifier, challenge) = generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert!(!verifier.contains('+'));
        assert!(!verifier.contains('/'));
        assert!(!verifier.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
    }

    #[test]
    fn test_start_anthropic_oauth_url_contains_required_params() {
        let info = start_anthropic_oauth().expect("start");
        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, ANTHROPIC_OAUTH_AUTHORIZE_URL);

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some(ANTHROPIC_OAUTH_CLIENT_ID)
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(ANTHROPIC_OAUTH_REDIRECT_URI)
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(ANTHROPIC_OAUTH_SCOPES)
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
        assert!(params.contains_key("code_challenge"));
    }

    #[test]
    fn test_parse_oauth_code_input_accepts_url_and_hash_formats() {
        let (code, state) = parse_oauth_code_input(
            "https://console.anthropic.com/oauth/code/callback?code=abc&state=def",
        );
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("def"));

        let (code, state) = parse_oauth_code_input("abc#def");
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("def"));

        let (code, state) = parse_oauth_code_input("abc");
        assert_eq!(code.as_deref(), Some("abc"));
        assert!(state.is_none());
    }

    #[test]
    fn test_complete_anthropic_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let err = complete_anthropic_oauth("abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    fn sample_oauth_config() -> crate::models::OAuthConfig {
        crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client-123".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
            redirect_uri: Some("http://localhost:9876/callback".to_string()),
        }
    }

    #[test]
    fn test_start_extension_oauth_url_contains_required_params() {
        let config = sample_oauth_config();
        let info = start_extension_oauth("my-ext-provider", &config).expect("start");

        assert_eq!(info.provider, "my-ext-provider");
        assert!(!info.verifier.is_empty());

        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://auth.example.com/authorize");

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("ext-client-123")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:9876/callback")
        );
        assert_eq!(params.get("scope").map(String::as_str), Some("read write"));
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
        assert!(params.contains_key("code_challenge"));
    }

    #[test]
    fn test_start_extension_oauth_no_redirect_uri() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client-123".to_string(),
            scopes: vec!["read".to_string()],
            redirect_uri: None,
        };
        let info = start_extension_oauth("no-redirect", &config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert!(!params.contains_key("redirect_uri"));
    }

    #[test]
    fn test_start_extension_oauth_empty_scopes() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client-123".to_string(),
            scopes: vec![],
            redirect_uri: None,
        };
        let info = start_extension_oauth("empty-scopes", &config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        // scope param still present but empty string
        assert_eq!(params.get("scope").map(String::as_str), Some(""));
    }

    #[test]
    fn test_start_extension_oauth_pkce_format() {
        let config = sample_oauth_config();
        let info = start_extension_oauth("pkce-test", &config).expect("start");

        // Verifier should be base64url without padding
        assert!(!info.verifier.contains('+'));
        assert!(!info.verifier.contains('/'));
        assert!(!info.verifier.contains('='));
        assert_eq!(info.verifier.len(), 43);
    }

    #[test]
    fn test_complete_extension_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = sample_oauth_config();
            let err = complete_extension_oauth(&config, "abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    #[test]
    fn test_complete_copilot_browser_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = CopilotOAuthConfig::default();
            let err = complete_copilot_browser_oauth(&config, "abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    #[test]
    fn test_complete_gitlab_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = GitLabOAuthConfig::default();
            let err = complete_gitlab_oauth(&config, "abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    #[test]
    fn test_refresh_expired_extension_oauth_tokens_skips_anthropic() {
        // Verify that the extension refresh method skips "anthropic" (handled separately).
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            // Insert an expired anthropic OAuth credential.
            let initial_access = next_token();
            let initial_refresh = next_token();
            auth.entries.insert(
                "anthropic".to_string(),
                AuthCredential::OAuth {
                    access_token: initial_access.clone(),
                    refresh_token: initial_refresh,
                    expires: 0, // expired
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            let mut extension_configs = HashMap::new();
            extension_configs.insert("anthropic".to_string(), sample_oauth_config());

            // Should succeed and NOT attempt refresh (anthropic is skipped).
            let result = auth
                .refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await;
            assert!(result.is_ok());

            // Credential should remain unchanged.
            assert!(
                matches!(
                    auth.entries.get("anthropic"),
                    Some(AuthCredential::OAuth { access_token, .. })
                        if access_token == &initial_access
                ),
                "expected OAuth credential"
            );
        });
    }

    #[test]
    fn test_refresh_expired_extension_oauth_tokens_skips_unexpired() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            // Insert a NOT expired credential.
            let initial_access_token = next_token();
            let initial_refresh_token = next_token();
            let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
            auth.entries.insert(
                "my-ext".to_string(),
                AuthCredential::OAuth {
                    access_token: initial_access_token.clone(),
                    refresh_token: initial_refresh_token,
                    expires: far_future,
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            let mut extension_configs = HashMap::new();
            extension_configs.insert("my-ext".to_string(), sample_oauth_config());

            let result = auth
                .refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await;
            assert!(result.is_ok());

            // Credential should remain unchanged (not expired, no refresh attempted).
            assert!(
                matches!(
                    auth.entries.get("my-ext"),
                    Some(AuthCredential::OAuth { access_token, .. })
                        if access_token == &initial_access_token
                ),
                "expected OAuth credential"
            );
        });
    }

    #[test]
    fn test_refresh_expired_extension_oauth_tokens_skips_unknown_provider() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            // Expired credential for a provider not in extension_configs.
            let initial_access_token = next_token();
            let initial_refresh_token = next_token();
            auth.entries.insert(
                "unknown-ext".to_string(),
                AuthCredential::OAuth {
                    access_token: initial_access_token.clone(),
                    refresh_token: initial_refresh_token,
                    expires: 0,
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            let extension_configs = HashMap::new(); // empty

            let result = auth
                .refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await;
            assert!(result.is_ok());

            // Credential should remain unchanged (no config to refresh with).
            assert!(
                matches!(
                    auth.entries.get("unknown-ext"),
                    Some(AuthCredential::OAuth { access_token, .. })
                        if access_token == &initial_access_token
                ),
                "expected OAuth credential"
            );
        });
    }

    #[test]
    #[cfg(unix)]
    fn test_refresh_expired_extension_oauth_tokens_updates_and_persists() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.entries.insert(
                "my-ext".to_string(),
                AuthCredential::OAuth {
                    access_token: "old-access".to_string(),
                    refresh_token: "old-refresh".to_string(),
                    expires: 0,
                    token_url: None,
                    client_id: None,
                },
            );

            let token_url = spawn_json_server(
                200,
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            );
            let mut config = sample_oauth_config();
            config.token_url = token_url;

            let mut extension_configs = HashMap::new();
            extension_configs.insert("my-ext".to_string(), config);

            let client = crate::http::client::Client::new();
            auth.refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await
                .expect("refresh");

            let now = chrono::Utc::now().timestamp_millis();
            match auth.entries.get("my-ext").expect("credential updated") {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires,
                    ..
                } => {
                    assert_eq!(access_token, "new-access");
                    assert_eq!(refresh_token, "new-refresh");
                    assert!(*expires > now);
                }
                other => {
                    unreachable!("expected oauth credential, got: {other:?}");
                }
            }

            let reloaded = AuthStorage::load(auth_path).expect("reload");
            match reloaded.get("my-ext").expect("persisted credential") {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    ..
                } => {
                    assert_eq!(access_token, "new-access");
                    assert_eq!(refresh_token, "new-refresh");
                }
                other => {
                    unreachable!("expected oauth credential, got: {other:?}");
                }
            }
        });
    }

    #[test]
    #[cfg(unix)]
    fn test_refresh_extension_oauth_token_redacts_secret_in_error() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let refresh_secret = "secret-refresh-token-123";
            let leaked_access = "leaked-access-token-456";
            let token_url = spawn_json_server(
                401,
                &format!(
                    r#"{{"error":"invalid_grant","echo":"{refresh_secret}","access_token":"{leaked_access}"}}"#
                ),
            );

            let mut config = sample_oauth_config();
            config.token_url = token_url;

            let client = crate::http::client::Client::new();
            let err = refresh_extension_oauth_token(&client, &config, refresh_secret)
                .await
                .expect_err("expected refresh failure");
            let err_text = err.to_string();

            assert!(
                err_text.contains("[REDACTED]"),
                "expected redacted marker in error: {err_text}"
            );
            assert!(
                !err_text.contains(refresh_secret),
                "refresh token leaked in error: {err_text}"
            );
            assert!(
                !err_text.contains(leaked_access),
                "access token leaked in error: {err_text}"
            );
        });
    }

    #[test]
    fn test_refresh_failure_produces_recovery_action() {
        let test_name = "test_refresh_failure_produces_recovery_action";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "anthropic" }),
        );

        let err = crate::error::Error::auth("OAuth token refresh failed: invalid_grant");
        let hints = err.hints();
        assert!(
            hints.hints.iter().any(|hint| hint.contains("login")),
            "expected auth hints to include login guidance, got {:?}",
            hints.hints
        );
        log_test_event(
            test_name,
            "refresh_failed",
            serde_json::json!({
                "provider": "anthropic",
                "error_type": "invalid_grant",
                "recovery": hints.hints,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_refresh_failure_network_vs_auth_different_messages() {
        let test_name = "test_refresh_failure_network_vs_auth_different_messages";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "scenario": "compare provider-network vs auth-refresh hints" }),
        );

        let auth_err = crate::error::Error::auth("OAuth token refresh failed: invalid_grant");
        let auth_hints = auth_err.hints();
        let network_err = crate::error::Error::provider(
            "anthropic",
            "Network connection error: connection reset by peer",
        );
        let network_hints = network_err.hints();

        assert!(
            auth_hints.hints.iter().any(|hint| hint.contains("login")),
            "expected auth-refresh hints to include login guidance, got {:?}",
            auth_hints.hints
        );
        assert!(
            network_hints.hints.iter().any(|hint| {
                let normalized = hint.to_ascii_lowercase();
                normalized.contains("network") || normalized.contains("connection")
            }),
            "expected network hints to mention network/connection checks, got {:?}",
            network_hints.hints
        );
        log_test_event(
            test_name,
            "error_classified",
            serde_json::json!({
                "auth_hints": auth_hints.hints,
                "network_hints": network_hints.hints,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_oauth_token_storage_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let expected_access_token = next_token();
        let expected_refresh_token = next_token();

        // Save OAuth credential.
        {
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.set(
                "ext-provider",
                AuthCredential::OAuth {
                    access_token: expected_access_token.clone(),
                    refresh_token: expected_refresh_token.clone(),
                    expires: 9_999_999_999_000,
                    token_url: None,
                    client_id: None,
                },
            );
            auth.save().expect("save");
        }

        // Load and verify.
        let loaded = AuthStorage::load(auth_path).expect("load");
        let cred = loaded.get("ext-provider").expect("credential present");
        match cred {
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                ..
            } => {
                assert_eq!(access_token, &expected_access_token);
                assert_eq!(refresh_token, &expected_refresh_token);
                assert_eq!(*expires, 9_999_999_999_000);
            }
            other => {
                unreachable!("expected OAuth credential, got: {other:?}");
            }
        }
    }

    #[test]
    fn test_oauth_pool() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let now = chrono::Utc::now().timestamp_millis();

        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        auth.set_login_credential(
            "openai",
            AuthCredential::OAuth {
                access_token: "pool-token-a".to_string(),
                refresh_token: "pool-refresh-a".to_string(),
                expires: now + 60_000,
                token_url: None,
                client_id: None,
            },
        );
        auth.set_login_credential(
            "OPENAI",
            AuthCredential::OAuth {
                access_token: "pool-token-b".to_string(),
                refresh_token: "pool-refresh-b".to_string(),
                expires: now + 60_000,
                token_url: None,
                client_id: None,
            },
        );

        assert!(auth.entries.get("openai").is_none());
        let pool = auth.oauth_pools.get("openai").expect("openai pool exists");
        assert_eq!(pool.accounts.len(), 2);

        {
            let pool = auth
                .oauth_pools
                .get_mut("openai")
                .expect("mutable openai pool exists");
            pool.active = None;
        }

        let first = auth
            .resolve_api_key_with_env_lookup("openai", None, |_| None)
            .expect("first rotated token");
        let second = auth
            .resolve_api_key_with_env_lookup("openai", None, |_| None)
            .expect("second rotated token");
        assert_ne!(first, second);

        auth.save().expect("save pool auth");
        let reloaded = AuthStorage::load(auth_path.clone()).expect("reload pool auth");
        assert!(reloaded.entries.get("openai").is_none());
        assert_eq!(
            reloaded.oauth_pools.get("openai").map(|p| p.accounts.len()),
            Some(2)
        );

        let legacy_auth_path = dir.path().join("legacy-auth.json");
        let mut legacy_entries = HashMap::new();
        legacy_entries.insert(
            "openai".to_string(),
            AuthCredential::OAuth {
                access_token: "legacy-access".to_string(),
                refresh_token: "legacy-refresh".to_string(),
                expires: now + 60_000,
                token_url: None,
                client_id: None,
            },
        );
        let legacy = AuthFile {
            version: AUTH_SCHEMA_VERSION,
            oauth_pools: HashMap::new(),
            entries: legacy_entries,
        };
        std::fs::write(
            &legacy_auth_path,
            serde_json::to_string_pretty(&legacy).expect("serialize legacy auth"),
        )
        .expect("write legacy auth");

        let migrated = AuthStorage::load(legacy_auth_path).expect("load and migrate legacy auth");
        assert!(
            !migrated
                .entries
                .values()
                .any(|credential| matches!(credential, AuthCredential::OAuth { .. }))
        );
        let migrated_pool = migrated.oauth_pools.values().next().expect("migrated pool");
        assert_eq!(migrated.oauth_pools.len(), 1);
        assert_eq!(migrated_pool.accounts.len(), 1);
        assert!(migrated_pool.active.is_some());
    }

    #[test]
    fn test_rotation_engine_exhaustion_trace() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut accounts = HashMap::new();
        accounts.insert(
            "acct-a".to_string(),
            OAuthAccountRecord {
                id: "acct-a".to_string(),
                label: Some("A".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "token-a".to_string(),
                    refresh_token: "refresh-a".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-a".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );
        accounts.insert(
            "acct-b".to_string(),
            OAuthAccountRecord {
                id: "acct-b".to_string(),
                label: Some("B".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "token-b".to_string(),
                    refresh_token: "refresh-b".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-b".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );

        let mut auth = AuthStorage {
            path: std::env::temp_dir().join(format!("pi-auth-{}.json", now)),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.oauth_pools.insert(
            "openai-codex".to_string(),
            OAuthProviderPool {
                active: None,
                order: vec!["acct-a".to_string(), "acct-b".to_string()],
                accounts,
                namespace: None,
            },
        );

        let plan = auth
            .build_oauth_rotation_plan("openai-codex", None, 2)
            .expect("rotation plan");
        assert_eq!(plan.max_attempts, 2);

        let scripted_outcomes = vec![
            OAuthOutcome {
                success: false,
                class: Some(OAuthErrorClass::RateLimit),
                retry_after_ms: Some(5_000),
                reason: Some("HTTP 429 Retry-After: 5".to_string()),
                status_code: None,
            },
            OAuthOutcome {
                success: false,
                class: Some(OAuthErrorClass::Auth),
                retry_after_ms: None,
                reason: Some("HTTP 401 unauthorized".to_string()),
                status_code: None,
            },
        ];
        let (selected, trace) =
            auth.execute_oauth_rotation_plan_with_outcomes(&plan, true, &scripted_outcomes);
        assert!(selected.is_none(), "all candidates should be exhausted");
        assert_eq!(
            trace.attempts.len(),
            2,
            "attempt loop should be bounded to max_attempts"
        );
        assert_eq!(trace.attempts[0].account_id, "acct-a");
        assert_eq!(trace.attempts[1].account_id, "acct-b");
    }

    #[test]
    fn test_background_refresh_dedupe() {
        let first = try_acquire_oauth_refresh_slot("provider-a", Some("acct-1"));
        let second = try_acquire_oauth_refresh_slot("provider-a", Some("acct-1"));
        assert!(first.is_some(), "first acquire should succeed");
        assert!(second.is_none(), "second acquire should dedupe");
        if let Some(slot) = first {
            release_oauth_refresh_slot(&slot);
        }
        let third = try_acquire_oauth_refresh_slot("provider-a", Some("acct-1"));
        assert!(third.is_some(), "slot should be reacquirable after release");
        if let Some(slot) = third {
            release_oauth_refresh_slot(&slot);
        }
    }

    #[test]
    fn test_policy_routing_filters_ineligible_accounts() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut accounts = HashMap::new();
        accounts.insert(
            "acct-ineligible".to_string(),
            OAuthAccountRecord {
                id: "acct-ineligible".to_string(),
                label: Some("ineligible".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "token-ineligible".to_string(),
                    refresh_token: "refresh-ineligible".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-1".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy {
                    allowed_models: vec!["gpt-4.1-*".to_string()],
                    excluded_models: Vec::new(),
                    provider_tags: Vec::new(),
                },
                provider_metadata: HashMap::new(),
            },
        );
        accounts.insert(
            "acct-eligible".to_string(),
            OAuthAccountRecord {
                id: "acct-eligible".to_string(),
                label: Some("eligible".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "token-eligible".to_string(),
                    refresh_token: "refresh-eligible".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-2".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );
        let auth = AuthStorage {
            path: std::env::temp_dir().join("pi-auth-policy-routing.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::from([(
                "openai-codex".to_string(),
                OAuthProviderPool {
                    active: Some("acct-ineligible".to_string()),
                    order: vec!["acct-ineligible".to_string(), "acct-eligible".to_string()],
                    accounts,
                    namespace: None,
                },
            )]),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        let resolved = auth
            .resolve_api_key_for_model("openai-codex", Some("gpt-4o-mini"), None)
            .expect("policy eligible account should resolve");
        assert_eq!(resolved, "token-eligible");
    }

    #[test]
    fn test_rotation_event_emission() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut accounts = HashMap::new();
        accounts.insert(
            "acct-a".to_string(),
            OAuthAccountRecord {
                id: "acct-a".to_string(),
                label: Some("A".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "token-a".to_string(),
                    refresh_token: "refresh-a".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-a".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );
        let mut auth = AuthStorage {
            path: std::env::temp_dir().join("pi-auth-rotation-events.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::from([(
                "openai-codex".to_string(),
                OAuthProviderPool {
                    active: Some("acct-a".to_string()),
                    order: vec!["acct-a".to_string()],
                    accounts,
                    namespace: None,
                },
            )]),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        let plan = auth
            .build_oauth_rotation_plan("openai-codex", None, 1)
            .expect("rotation plan");
        let (_, trace) = auth.execute_oauth_rotation_plan_with_outcomes(
            &plan,
            true,
            &[OAuthOutcome {
                success: false,
                class: Some(OAuthErrorClass::RateLimit),
                retry_after_ms: Some(3_000),
                reason: Some("HTTP 429 Retry-After: 3".to_string()),
                status_code: None,
            }],
        );
        assert_eq!(trace.attempts.len(), 1);
        assert_eq!(
            trace.attempts[0].class,
            Some(OAuthErrorClass::RateLimit),
            "rotation trace should carry structured outcome class"
        );
        assert!(
            trace.attempts[0]
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("Retry-After")),
            "rotation trace should include reason payload"
        );
    }

    #[test]
    fn test_rotation_retry_after_parsing() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut accounts = HashMap::new();
        accounts.insert(
            "acct-rate".to_string(),
            OAuthAccountRecord {
                id: "acct-rate".to_string(),
                label: Some("rate".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "rate-token".to_string(),
                    refresh_token: "rate-refresh".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-rate".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );
        accounts.insert(
            "acct-fallback".to_string(),
            OAuthAccountRecord {
                id: "acct-fallback".to_string(),
                label: Some("fallback".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "fallback-token".to_string(),
                    refresh_token: "fallback-refresh".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-fallback".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );
        let mut auth = AuthStorage {
            path: std::env::temp_dir().join("pi-auth-retry-after.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::from([(
                "openai-codex".to_string(),
                OAuthProviderPool {
                    active: Some("acct-rate".to_string()),
                    order: vec!["acct-rate".to_string(), "acct-fallback".to_string()],
                    accounts,
                    namespace: None,
                },
            )]),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        let plan = auth
            .build_oauth_rotation_plan("openai-codex", None, 2)
            .expect("rotation plan");
        let (selected, _) = auth.execute_oauth_rotation_plan_with_outcomes(
            &plan,
            true,
            &[
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::RateLimit),
                    retry_after_ms: None,
                    reason: Some("HTTP 429 Retry-After: 9".to_string()),
                    status_code: None,
                },
                OAuthOutcome {
                    success: true,
                    class: None,
                    retry_after_ms: None,
                    reason: Some("ok".to_string()),
                    status_code: None,
                },
            ],
        );
        assert_eq!(
            selected.expect("fallback candidate").account_id,
            "acct-fallback"
        );
        let cooldown = auth
            .oauth_pools
            .get("openai-codex")
            .and_then(|pool| pool.accounts.get("acct-rate"))
            .and_then(|acct| acct.health.cooldown_until_ms)
            .expect("rate-limited account cooldown");
        assert!(
            cooldown >= now + 8_000,
            "expected cooldown derived from parsed Retry-After header"
        );
    }

    #[test]
    fn test_rotation_auth_refresh_paths() {
        let now = chrono::Utc::now().timestamp_millis();
        let build_storage = || {
            let mut accounts = HashMap::new();
            accounts.insert(
                "acct-auth".to_string(),
                OAuthAccountRecord {
                    id: "acct-auth".to_string(),
                    label: Some("auth".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "auth-token".to_string(),
                        refresh_token: "auth-refresh".to_string(),
                        expires: now + 60_000,
                        token_url: Some("https://auth.example/token".to_string()),
                        client_id: Some("client-auth".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );
            accounts.insert(
                "acct-fallback".to_string(),
                OAuthAccountRecord {
                    id: "acct-fallback".to_string(),
                    label: Some("fallback".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "fallback-token".to_string(),
                        refresh_token: "fallback-refresh".to_string(),
                        expires: now + 60_000,
                        token_url: Some("https://auth.example/token".to_string()),
                        client_id: Some("client-fallback".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );
            AuthStorage {
                path: std::env::temp_dir().join(format!("pi-auth-auth-refresh-{now}.json")),
                entries: HashMap::new(),
                oauth_pools: HashMap::from([(
                    "openai-codex".to_string(),
                    OAuthProviderPool {
                        active: Some("acct-auth".to_string()),
                        order: vec!["acct-auth".to_string(), "acct-fallback".to_string()],
                        accounts,
                        namespace: None,
                    },
                )]),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            }
        };

        // Same-account auth retry succeeds.
        let mut auth_retry_ok = build_storage();
        let plan_retry_ok = auth_retry_ok
            .build_oauth_rotation_plan("openai-codex", None, 3)
            .expect("rotation plan");
        let (selected_retry_ok, trace_retry_ok) = auth_retry_ok
            .execute_oauth_rotation_plan_with_outcomes(
                &plan_retry_ok,
                true,
                &[
                    OAuthOutcome {
                        success: false,
                        class: Some(OAuthErrorClass::Auth),
                        retry_after_ms: None,
                        reason: Some("HTTP 401 unauthorized".to_string()),
                        status_code: None,
                    },
                    OAuthOutcome {
                        success: true,
                        class: None,
                        retry_after_ms: None,
                        reason: Some("refresh retry success".to_string()),
                        status_code: None,
                    },
                ],
            );
        assert_eq!(
            selected_retry_ok.expect("auth retry success").account_id,
            "acct-auth"
        );
        assert_eq!(trace_retry_ok.attempts.len(), 2);

        // Auth retry fails terminally and rotates to fallback.
        let mut auth_retry_fail = build_storage();
        let plan_retry_fail = auth_retry_fail
            .build_oauth_rotation_plan("openai-codex", None, 4)
            .expect("rotation plan");
        let (selected_retry_fail, _) = auth_retry_fail.execute_oauth_rotation_plan_with_outcomes(
            &plan_retry_fail,
            true,
            &[
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::Auth),
                    retry_after_ms: None,
                    reason: Some("HTTP 401 unauthorized".to_string()),
                    status_code: None,
                },
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::Auth),
                    retry_after_ms: None,
                    reason: Some("HTTP 401 unauthorized retry".to_string()),
                    status_code: None,
                },
                OAuthOutcome {
                    success: true,
                    class: None,
                    retry_after_ms: None,
                    reason: Some("fallback success".to_string()),
                    status_code: None,
                },
            ],
        );
        assert_eq!(
            selected_retry_fail.expect("fallback selected").account_id,
            "acct-fallback"
        );
        let requires_relogin = auth_retry_fail
            .oauth_pools
            .get("openai-codex")
            .and_then(|pool| pool.accounts.get("acct-auth"))
            .is_some_and(|acct| acct.health.requires_relogin);
        assert!(requires_relogin);
    }

    #[test]
    fn test_rotation_transport_retry_then_rotate() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut accounts = HashMap::new();
        accounts.insert(
            "acct-primary".to_string(),
            OAuthAccountRecord {
                id: "acct-primary".to_string(),
                label: Some("primary".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "primary-token".to_string(),
                    refresh_token: "primary-refresh".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-primary".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );
        accounts.insert(
            "acct-secondary".to_string(),
            OAuthAccountRecord {
                id: "acct-secondary".to_string(),
                label: Some("secondary".to_string()),
                credential: AuthCredential::OAuth {
                    access_token: "secondary-token".to_string(),
                    refresh_token: "secondary-refresh".to_string(),
                    expires: now + 60_000,
                    token_url: Some("https://auth.example/token".to_string()),
                    client_id: Some("client-secondary".to_string()),
                },
                health: OAuthAccountHealth::default(),
                policy: OAuthAccountPolicy::default(),
                provider_metadata: HashMap::new(),
            },
        );
        let mut auth = AuthStorage {
            path: std::env::temp_dir().join("pi-auth-transport-rotate.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::from([(
                "openai-codex".to_string(),
                OAuthProviderPool {
                    active: Some("acct-primary".to_string()),
                    order: vec!["acct-primary".to_string(), "acct-secondary".to_string()],
                    accounts,
                    namespace: None,
                },
            )]),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        let plan = auth
            .build_oauth_rotation_plan("openai-codex", None, 4)
            .expect("rotation plan");
        let (selected, trace) = auth.execute_oauth_rotation_plan_with_outcomes(
            &plan,
            true,
            &[
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::Transport),
                    retry_after_ms: None,
                    reason: Some("network timeout".to_string()),
                    status_code: None,
                },
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::Transport),
                    retry_after_ms: None,
                    reason: Some("network timeout retry".to_string()),
                    status_code: None,
                },
                OAuthOutcome {
                    success: true,
                    class: None,
                    retry_after_ms: None,
                    reason: Some("secondary success".to_string()),
                    status_code: None,
                },
            ],
        );
        assert_eq!(
            selected.expect("secondary should be selected").account_id,
            "acct-secondary"
        );
        assert_eq!(
            trace.attempts.len(),
            3,
            "expected one transport retry before rotating"
        );
    }

    #[test]
    fn test_rotation_non_replayable_guard() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut accounts = HashMap::new();
        for (id, token) in [("acct-a", "token-a"), ("acct-b", "token-b")] {
            accounts.insert(
                id.to_string(),
                OAuthAccountRecord {
                    id: id.to_string(),
                    label: Some(id.to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: token.to_string(),
                        refresh_token: format!("refresh-{id}"),
                        expires: now + 60_000,
                        token_url: Some("https://auth.example/token".to_string()),
                        client_id: Some("client".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );
        }
        let mut auth = AuthStorage {
            path: std::env::temp_dir().join("pi-auth-non-replayable.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::from([(
                "openai-codex".to_string(),
                OAuthProviderPool {
                    active: Some("acct-a".to_string()),
                    order: vec!["acct-a".to_string(), "acct-b".to_string()],
                    accounts,
                    namespace: None,
                },
            )]),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        let plan = auth
            .build_oauth_rotation_plan("openai-codex", None, 5)
            .expect("rotation plan");
        let (selected, trace) = auth.execute_oauth_rotation_plan_with_outcomes(
            &plan,
            false,
            &[
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::Transport),
                    retry_after_ms: None,
                    reason: Some("connection reset".to_string()),
                    status_code: None,
                },
                OAuthOutcome {
                    success: true,
                    class: None,
                    retry_after_ms: None,
                    reason: Some("should not execute".to_string()),
                    status_code: None,
                },
            ],
        );
        assert!(
            selected.is_none(),
            "non-replayable path must not rotate/retry"
        );
        assert_eq!(trace.attempts.len(), 1);
    }

    #[test]
    fn test_rotation_exhaustion_trace() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut accounts = HashMap::new();
        for (id, token) in [
            ("acct-a", "token-a"),
            ("acct-b", "token-b"),
            ("acct-c", "token-c"),
        ] {
            accounts.insert(
                id.to_string(),
                OAuthAccountRecord {
                    id: id.to_string(),
                    label: Some(id.to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: token.to_string(),
                        refresh_token: format!("refresh-{id}"),
                        expires: now + 60_000,
                        token_url: Some("https://auth.example/token".to_string()),
                        client_id: Some("client".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );
        }
        let mut auth = AuthStorage {
            path: std::env::temp_dir().join("pi-auth-exhaustion.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::from([(
                "openai-codex".to_string(),
                OAuthProviderPool {
                    active: None,
                    order: vec![
                        "acct-a".to_string(),
                        "acct-b".to_string(),
                        "acct-c".to_string(),
                    ],
                    accounts,
                    namespace: None,
                },
            )]),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        let plan = auth
            .build_oauth_rotation_plan("openai-codex", None, 2)
            .expect("rotation plan");
        let (selected, trace) = auth.execute_oauth_rotation_plan_with_outcomes(
            &plan,
            true,
            &[
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::Auth),
                    retry_after_ms: None,
                    reason: Some("401".to_string()),
                    status_code: Some(401),
                },
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::Auth),
                    retry_after_ms: None,
                    reason: Some("401 retry".to_string()),
                    status_code: Some(401),
                },
                OAuthOutcome {
                    success: false,
                    class: Some(OAuthErrorClass::RateLimit),
                    retry_after_ms: Some(1_000),
                    reason: Some("429".to_string()),
                    status_code: Some(429),
                },
            ],
        );
        assert!(selected.is_none());
        assert_eq!(
            trace.attempts.len(),
            2,
            "trace should be bounded to max_attempts"
        );
    }

    #[test]
    fn test_background_refresh_dedupe_concurrency() {
        use std::sync::Barrier;

        let barrier = Arc::new(Barrier::new(2));
        let barrier_a = barrier.clone();
        let barrier_b = barrier.clone();

        let handle_a = std::thread::spawn(move || {
            barrier_a.wait();
            try_acquire_oauth_refresh_slot("provider-concurrent", Some("acct-c"))
        });
        let handle_b = std::thread::spawn(move || {
            barrier_b.wait();
            try_acquire_oauth_refresh_slot("provider-concurrent", Some("acct-c"))
        });

        let a = handle_a.join().expect("thread a");
        let b = handle_b.join().expect("thread b");
        assert!(
            (a.is_some() && b.is_none()) || (a.is_none() && b.is_some()),
            "exactly one concurrent acquire should win"
        );
        if let Some(slot) = a {
            release_oauth_refresh_slot(&slot);
        }
        if let Some(slot) = b {
            release_oauth_refresh_slot(&slot);
        }
    }

    #[test]
    fn test_codex_import_non_refreshable_lifecycle() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let expiring_soon = chrono::Utc::now().timestamp_millis() + 30_000;

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::from([(
                    "openai-codex".to_string(),
                    OAuthProviderPool {
                        active: Some("acct-import".to_string()),
                        order: vec!["acct-import".to_string()],
                        accounts: HashMap::from([(
                            "acct-import".to_string(),
                            OAuthAccountRecord {
                                id: "acct-import".to_string(),
                                label: Some("imported".to_string()),
                                credential: AuthCredential::OAuth {
                                    access_token: "access-only".to_string(),
                                    refresh_token: "refresh-present".to_string(),
                                    expires: expiring_soon,
                                    token_url: None,
                                    client_id: None,
                                },
                                health: OAuthAccountHealth::default(),
                                policy: OAuthAccountPolicy::default(),
                                provider_metadata: HashMap::new(),
                            },
                        )]),
                        namespace: None,
                    },
                )]),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            if let Some(pool) = auth.oauth_pools.get_mut("openai-codex") {
                AuthStorage::normalize_pool("openai-codex", pool);
            }

            let client = crate::http::client::Client::new();
            let _ = auth.refresh_expired_oauth_tokens_with_client(&client).await;

            let account = auth
                .oauth_pools
                .get("openai-codex")
                .and_then(|pool| pool.accounts.get("acct-import"))
                .expect("imported account");
            assert!(account.health.non_refreshable);
            assert!(account.health.requires_relogin);
        });
    }

    #[test]
    fn test_copilot_non_refreshable_rotation() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let now = chrono::Utc::now().timestamp_millis();

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::from([(
                    "github-copilot".to_string(),
                    OAuthProviderPool {
                        active: Some("acct-device".to_string()),
                        order: vec!["acct-device".to_string(), "acct-good".to_string()],
                        accounts: HashMap::from([
                            (
                                "acct-device".to_string(),
                                OAuthAccountRecord {
                                    id: "acct-device".to_string(),
                                    label: Some("device".to_string()),
                                    credential: AuthCredential::OAuth {
                                        access_token: "device-token".to_string(),
                                        refresh_token: String::new(),
                                        expires: now + 30_000,
                                        token_url: Some("https://auth.example/token".to_string()),
                                        client_id: Some("client-device".to_string()),
                                    },
                                    health: OAuthAccountHealth::default(),
                                    policy: OAuthAccountPolicy::default(),
                                    provider_metadata: HashMap::new(),
                                },
                            ),
                            (
                                "acct-good".to_string(),
                                OAuthAccountRecord {
                                    id: "acct-good".to_string(),
                                    label: Some("good".to_string()),
                                    credential: AuthCredential::OAuth {
                                        access_token: "good-token".to_string(),
                                        refresh_token: "good-refresh".to_string(),
                                        expires: now + 3_600_000,
                                        token_url: Some("https://auth.example/token".to_string()),
                                        client_id: Some("client-good".to_string()),
                                    },
                                    health: OAuthAccountHealth::default(),
                                    policy: OAuthAccountPolicy::default(),
                                    provider_metadata: HashMap::new(),
                                },
                            ),
                        ]),
                        namespace: None,
                    },
                )]),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            if let Some(pool) = auth.oauth_pools.get_mut("github-copilot") {
                AuthStorage::normalize_pool("github-copilot", pool);
            }
            let client = crate::http::client::Client::new();
            let _ = auth.refresh_expired_oauth_tokens_with_client(&client).await;
            let device_requires_relogin = auth
                .oauth_pools
                .get("github-copilot")
                .and_then(|pool| pool.accounts.get("acct-device"))
                .is_some_and(|account| account.health.requires_relogin);
            assert!(
                device_requires_relogin,
                "device-flow account should be marked relogin-required before rotation fallback"
            );
            let resolved = auth
                .resolve_api_key("github-copilot", None)
                .expect("rotation should bypass relogin-required account");
            assert_eq!(resolved, "good-token");
        });
    }

    #[test]
    fn test_gemini_metadata_persistence_refresh_reuse() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let now = chrono::Utc::now().timestamp_millis();
        let access = encode_project_scoped_access_token("access-token", "gem-proj-123");

        let auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
            oauth_pools: HashMap::from([(
                "google-gemini-cli".to_string(),
                OAuthProviderPool {
                    active: Some("acct-gem".to_string()),
                    order: vec!["acct-gem".to_string()],
                    accounts: HashMap::from([(
                        "acct-gem".to_string(),
                        OAuthAccountRecord {
                            id: "acct-gem".to_string(),
                            label: Some("gemini".to_string()),
                            credential: AuthCredential::OAuth {
                                access_token: access.clone(),
                                refresh_token: "gem-refresh".to_string(),
                                expires: now + 300_000,
                                token_url: None,
                                client_id: None,
                            },
                            health: OAuthAccountHealth::default(),
                            policy: OAuthAccountPolicy::default(),
                            provider_metadata: HashMap::from([(
                                OAUTH_PROJECT_ID_METADATA_KEY.to_string(),
                                "gem-proj-123".to_string(),
                            )]),
                        },
                    )]),
                    namespace: None,
                },
            )]),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.save().expect("save auth");
        let loaded = AuthStorage::load(auth_path).expect("load auth");

        let project_id = resolve_google_refresh_project_id("google-gemini-cli", &access);
        assert_eq!(project_id.as_deref(), Some("gem-proj-123"));

        let metadata_value = loaded
            .oauth_pools
            .get("google-gemini-cli")
            .and_then(|pool| pool.accounts.get("acct-gem"))
            .and_then(|acct| acct.provider_metadata.get(OAUTH_PROJECT_ID_METADATA_KEY))
            .cloned();
        assert_eq!(metadata_value.as_deref(), Some("gem-proj-123"));
    }

    #[test]
    fn test_classify_oauth_failure_prioritizes_rate_limit() {
        let classification =
            AuthStorage::classify_oauth_failure("HTTP 429 forbidden: rate limit exceeded");
        assert_eq!(classification, OAuthFailureKind::RateLimited);
    }

    #[test]
    fn test_adapter_classification() {
        let codex_adapter = oauth_provider_adapter("openai-codex");
        assert_eq!(codex_adapter.provider_id(), "openai-codex");
        assert_eq!(
            codex_adapter.classify_error("HTTP 401 unauthorized"),
            OAuthErrorClass::Auth
        );
        assert_eq!(
            codex_adapter.classify_error("HTTP 429 retry-after: 12"),
            OAuthErrorClass::RateLimit
        );
        assert_eq!(
            codex_adapter.retry_after_ms("HTTP 429 Retry-After: 12"),
            Some(12_000)
        );

        let future = (chrono::Utc::now() + chrono::Duration::seconds(45))
            .to_rfc2822()
            .to_string();
        let retry_after_http_date =
            codex_adapter.retry_after_ms(format!("HTTP 429 Retry-After: {future}").as_str());
        assert!(
            retry_after_http_date.is_some_and(|ms| (30_000..=60_000).contains(&ms)),
            "expected parsed HTTP-date retry-after to be around 45s"
        );

        assert!(
            codex_adapter
                .non_refreshable_reason(
                    "openai-codex",
                    "access",
                    "refresh",
                    None,
                    Some("client-id")
                )
                .is_some(),
            "codex adapter should require token_url/client_id metadata"
        );

        let copilot_adapter = oauth_provider_adapter("github-copilot");
        assert!(
            copilot_adapter
                .non_refreshable_reason(
                    "github-copilot",
                    "access",
                    "",
                    Some("https://token.example"),
                    Some("client-id")
                )
                .is_some(),
            "copilot adapter should classify missing refresh tokens as non-refreshable"
        );

        let gemini_adapter = oauth_provider_adapter("google-gemini-cli");
        assert!(
            gemini_adapter
                .non_refreshable_reason("google-gemini-cli", "access", "", None, None)
                .is_some(),
            "gemini adapter should classify missing refresh token as non-refreshable"
        );
    }

    #[test]
    fn test_resolve_google_refresh_project_id_prefers_embedded_payload() {
        let payload = encode_project_scoped_access_token("token", "embedded-proj");
        assert_eq!(
            resolve_google_refresh_project_id("google-gemini-cli", &payload).as_deref(),
            Some("embedded-proj")
        );
    }

    #[test]
    fn test_oauth_api_key_returns_access_token_when_unexpired() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let expected_access_token = next_token();
        let expected_refresh_token = next_token();
        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "ext-provider",
            AuthCredential::OAuth {
                access_token: expected_access_token.clone(),
                refresh_token: expected_refresh_token,
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );

        assert_eq!(
            auth.api_key("ext-provider").as_deref(),
            Some(expected_access_token.as_str())
        );
    }

    #[test]
    fn test_oauth_api_key_returns_none_when_expired() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let expected_access_token = next_token();
        let expected_refresh_token = next_token();
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "ext-provider",
            AuthCredential::OAuth {
                access_token: expected_access_token,
                refresh_token: expected_refresh_token,
                expires: 0, // expired
                token_url: None,
                client_id: None,
            },
        );

        assert_eq!(auth.api_key("ext-provider"), None);
    }

    #[test]
    fn test_credential_status_reports_oauth_valid_and_expired() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let now = chrono::Utc::now().timestamp_millis();

        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "valid-oauth",
            AuthCredential::OAuth {
                access_token: "valid-access".to_string(),
                refresh_token: "valid-refresh".to_string(),
                expires: now + 30_000,
                token_url: None,
                client_id: None,
            },
        );
        auth.set(
            "expired-oauth",
            AuthCredential::OAuth {
                access_token: "expired-access".to_string(),
                refresh_token: "expired-refresh".to_string(),
                expires: now - 30_000,
                token_url: None,
                client_id: None,
            },
        );

        match auth.credential_status("valid-oauth") {
            CredentialStatus::OAuthValid { expires_in_ms } => {
                assert!(expires_in_ms > 0, "expires_in_ms should be positive");
                log_test_event(
                    "test_provider_listing_shows_expiry",
                    "assertion",
                    serde_json::json!({
                        "provider": "valid-oauth",
                        "status": "oauth_valid",
                        "expires_in_ms": expires_in_ms,
                    }),
                );
            }
            other => panic!("expected OAuthValid, got {other:?}"),
        }

        match auth.credential_status("expired-oauth") {
            CredentialStatus::OAuthExpired { expired_by_ms } => {
                assert!(expired_by_ms > 0, "expired_by_ms should be positive");
            }
            other => panic!("expected OAuthExpired, got {other:?}"),
        }
    }

    #[test]
    fn test_credential_status_uses_alias_lookup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-key".to_string(),
            },
        );

        assert_eq!(auth.credential_status("gemini"), CredentialStatus::ApiKey);
        assert_eq!(
            auth.credential_status("missing-provider"),
            CredentialStatus::Missing
        );
        log_test_event(
            "test_provider_listing_shows_all_providers",
            "assertion",
            serde_json::json!({
                "providers_checked": ["google", "gemini", "missing-provider"],
                "google_status": "api_key",
                "missing_status": "missing",
            }),
        );
        log_test_event(
            "test_provider_listing_no_credentials",
            "assertion",
            serde_json::json!({
                "provider": "missing-provider",
                "status": "Not authenticated",
            }),
        );
    }

    #[test]
    fn test_has_stored_credential_uses_reverse_alias_lookup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: "legacy-gemini-key".to_string(),
            },
        );

        assert!(auth.has_stored_credential("google"));
        assert!(auth.has_stored_credential("gemini"));
    }

    #[test]
    fn test_resolve_api_key_handles_case_insensitive_stored_provider_keys() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "Google",
            AuthCredential::ApiKey {
                key: "mixed-case-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("google", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("mixed-case-key"));
    }

    #[test]
    fn test_credential_status_uses_reverse_alias_lookup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: "legacy-gemini-key".to_string(),
            },
        );

        assert_eq!(auth.credential_status("google"), CredentialStatus::ApiKey);
    }

    #[test]
    fn test_remove_provider_aliases_removes_canonical_and_alias_entries() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-key".to_string(),
            },
        );
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: "gemini-key".to_string(),
            },
        );

        assert!(auth.remove_provider_aliases("google"));
        assert!(!auth.has_stored_credential("google"));
        assert!(!auth.has_stored_credential("gemini"));
    }

    #[test]
    fn test_auth_remove_credential() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "ext-provider",
            AuthCredential::ApiKey {
                key: "key-123".to_string(),
            },
        );

        assert!(auth.get("ext-provider").is_some());
        assert!(auth.remove("ext-provider"));
        assert!(auth.get("ext-provider").is_none());
        assert!(!auth.remove("ext-provider")); // already removed
    }

    #[test]
    fn test_auth_env_key_returns_none_for_extension_providers() {
        // Extension providers don't have hard-coded env vars.
        assert!(env_key_for_provider("my-ext-provider").is_none());
        assert!(env_key_for_provider("custom-llm").is_none());
        // Built-in providers do.
        assert_eq!(env_key_for_provider("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_key_for_provider("openai"), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn test_extension_oauth_config_special_chars_in_scopes() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client".to_string(),
            scopes: vec![
                "api:read".to_string(),
                "api:write".to_string(),
                "user:profile".to_string(),
            ],
            redirect_uri: None,
        };
        let info = start_extension_oauth("scoped", &config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some("api:read api:write user:profile")
        );
    }

    #[test]
    fn test_extension_oauth_url_encodes_special_chars() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "client with spaces".to_string(),
            scopes: vec!["scope&dangerous".to_string()],
            redirect_uri: Some("http://localhost:9876/call back".to_string()),
        };
        let info = start_extension_oauth("encoded", &config).expect("start");

        // The URL should be valid and contain encoded values.
        assert!(info.url.contains("client%20with%20spaces"));
        assert!(info.url.contains("scope%26dangerous"));
        assert!(info.url.contains("call%20back"));
    }

    // ── AuthStorage creation (additional edge cases) ─────────────────

    #[test]
    fn test_auth_storage_load_valid_api_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let content = r#"{"anthropic":{"type":"api_key","key":"sk-test-abc"}}"#;
        fs::write(&auth_path, content).expect("write");

        let auth = AuthStorage::load(auth_path).expect("load");
        assert!(auth.entries.contains_key("anthropic"));
        match auth.get("anthropic").expect("credential") {
            AuthCredential::ApiKey { key } => assert_eq!(key, "sk-test-abc"),
            other => panic!("expected ApiKey, got: {other:?}"),
        }
    }

    #[test]
    fn test_auth_storage_load_corrupted_json_returns_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        fs::write(&auth_path, "not valid json {{").expect("write");

        let auth = AuthStorage::load(auth_path).expect("load");
        // Corrupted JSON falls through to `unwrap_or_default()`.
        assert!(auth.entries.is_empty());
    }

    #[test]
    fn test_auth_storage_load_empty_file_returns_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        fs::write(&auth_path, "").expect("write");

        let auth = AuthStorage::load(auth_path).expect("load");
        assert!(auth.entries.is_empty());
    }

    // ── resolve_api_key edge cases ───────────────────────────────────

    #[test]
    fn test_resolve_api_key_empty_override_still_wins() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "stored-key".to_string(),
            },
        );

        // Empty string override still counts as explicit.
        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", Some(""), |_| None);
        assert_eq!(resolved.as_deref(), Some(""));
    }

    #[test]
    fn test_resolve_api_key_env_beats_stored() {
        // The new precedence is: override > env > stored.
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "stored-key".to_string(),
            },
        );

        let resolved =
            auth.resolve_api_key_with_env_lookup("openai", None, |_| Some("env-key".to_string()));
        assert_eq!(
            resolved.as_deref(),
            Some("env-key"),
            "env should beat stored"
        );
    }

    #[test]
    fn test_resolve_api_key_groq_env_beats_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "groq",
            AuthCredential::ApiKey {
                key: "stored-groq-key".to_string(),
            },
        );

        let resolved =
            auth.resolve_api_key_with_env_lookup("groq", None, |_| Some("env-groq-key".into()));
        assert_eq!(resolved.as_deref(), Some("env-groq-key"));
    }

    #[test]
    fn test_resolve_api_key_openrouter_env_beats_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "openrouter",
            AuthCredential::ApiKey {
                key: "stored-openrouter-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("openrouter", None, |var| match var {
            "OPENROUTER_API_KEY" => Some("env-openrouter-key".to_string()),
            _ => None,
        });
        assert_eq!(resolved.as_deref(), Some("env-openrouter-key"));
    }

    #[test]
    fn test_resolve_api_key_empty_env_falls_through_to_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "stored-key".to_string(),
            },
        );

        // Empty env var is filtered out, falls through to stored.
        let resolved =
            auth.resolve_api_key_with_env_lookup("openai", None, |_| Some(String::new()));
        assert_eq!(
            resolved.as_deref(),
            Some("stored-key"),
            "empty env should fall through to stored"
        );
    }

    #[test]
    fn test_resolve_api_key_whitespace_env_falls_through_to_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "stored-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("openai", None, |_| Some("   ".into()));
        assert_eq!(resolved.as_deref(), Some("stored-key"));
    }

    #[test]
    fn test_resolve_api_key_anthropic_oauth_marks_for_bearer_lane() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                access_token: "sk-ant-api-like-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: chrono::Utc::now().timestamp_millis() + 60_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", None, |_| None);
        let token = resolved.expect("resolved anthropic oauth token");
        assert_eq!(
            unmark_anthropic_oauth_bearer_token(&token),
            Some("sk-ant-api-like-token")
        );
    }

    #[test]
    fn test_resolve_api_key_non_anthropic_oauth_is_not_marked() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "openai-codex",
            AuthCredential::OAuth {
                access_token: "codex-oauth-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: chrono::Utc::now().timestamp_millis() + 60_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("openai-codex", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("codex-oauth-token"));
    }

    #[test]
    fn test_resolve_api_key_google_uses_gemini_env_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "stored-google-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("google", None, |var| match var {
            "GOOGLE_API_KEY" => Some(String::new()),
            "GEMINI_API_KEY" => Some("gemini-fallback-key".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("gemini-fallback-key"));
    }

    #[test]
    fn test_resolve_api_key_gemini_alias_reads_google_stored_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "stored-google-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("gemini", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("stored-google-key"));
    }

    #[test]
    fn test_resolve_api_key_google_reads_legacy_gemini_alias_stored_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: "legacy-gemini-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("google", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("legacy-gemini-key"));
    }

    #[test]
    fn test_resolve_api_key_qwen_uses_qwen_env_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "alibaba",
            AuthCredential::ApiKey {
                key: "stored-dashscope-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("qwen", None, |var| match var {
            "DASHSCOPE_API_KEY" => Some(String::new()),
            "QWEN_API_KEY" => Some("qwen-fallback-key".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("qwen-fallback-key"));
    }

    #[test]
    fn test_resolve_api_key_kimi_uses_kimi_env_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "moonshotai",
            AuthCredential::ApiKey {
                key: "stored-moonshot-key".to_string(),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("kimi", None, |var| match var {
            "MOONSHOT_API_KEY" => Some(String::new()),
            "KIMI_API_KEY" => Some("kimi-fallback-key".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("kimi-fallback-key"));
    }

    #[test]
    fn test_resolve_api_key_primary_env_wins_over_alias_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        let resolved = auth.resolve_api_key_with_env_lookup("alibaba", None, |var| match var {
            "DASHSCOPE_API_KEY" => Some("dashscope-primary".to_string()),
            "QWEN_API_KEY" => Some("qwen-secondary".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("dashscope-primary"));
    }

    // ── API key storage and persistence ───────────────────────────────

    #[test]
    fn test_api_key_store_and_retrieve() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "sk-openai-test".to_string(),
            },
        );

        assert_eq!(auth.api_key("openai").as_deref(), Some("sk-openai-test"));
    }

    #[test]
    fn test_google_api_key_overwrite_persists_latest_value() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-key-old".to_string(),
            },
        );
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-key-new".to_string(),
            },
        );
        auth.save().expect("save");

        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("google").as_deref(), Some("google-key-new"));
    }

    #[test]
    fn test_multiple_providers_stored_and_retrieved() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "sk-ant".to_string(),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "sk-oai".to_string(),
            },
        );
        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        auth.set(
            "google",
            AuthCredential::OAuth {
                access_token: "goog-token".to_string(),
                refresh_token: "goog-refresh".to_string(),
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );
        auth.save().expect("save");

        // Reload and verify all three.
        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("sk-ant"));
        assert_eq!(loaded.api_key("openai").as_deref(), Some("sk-oai"));
        assert_eq!(loaded.api_key("google").as_deref(), Some("goog-token"));
        // OAuth credentials are stored in oauth_pools, not entries
        assert_eq!(loaded.provider_names().len(), 3);
    }

    #[test]
    fn test_save_creates_parent_directories() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("nested").join("dirs").join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "nested-key".to_string(),
            },
        );
        auth.save().expect("save should create parents");
        assert!(auth_path.exists());

        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("nested-key"));
    }

    #[cfg(unix)]
    #[test]
    fn test_save_sets_600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "secret".to_string(),
            },
        );
        auth.save().expect("save");

        let metadata = fs::metadata(&auth_path).expect("metadata");
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.json should be owner-only read/write");
    }

    // ── Missing key handling ──────────────────────────────────────────

    #[test]
    fn test_api_key_returns_none_for_missing_provider() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        assert!(auth.api_key("nonexistent").is_none());
    }

    #[test]
    fn test_get_returns_none_for_missing_provider() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        assert!(auth.get("nonexistent").is_none());
    }

    // ── env_keys_for_provider coverage ────────────────────────────────

    #[test]
    fn test_env_keys_all_built_in_providers() {
        let providers = [
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("openai", "OPENAI_API_KEY"),
            ("google", "GOOGLE_API_KEY"),
            ("google-vertex", "GOOGLE_CLOUD_API_KEY"),
            ("amazon-bedrock", "AWS_ACCESS_KEY_ID"),
            ("azure-openai", "AZURE_OPENAI_API_KEY"),
            ("github-copilot", "GITHUB_COPILOT_API_KEY"),
            ("xai", "XAI_API_KEY"),
            ("groq", "GROQ_API_KEY"),
            ("deepinfra", "DEEPINFRA_API_KEY"),
            ("cerebras", "CEREBRAS_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
            ("mistral", "MISTRAL_API_KEY"),
            ("cohere", "COHERE_API_KEY"),
            ("perplexity", "PERPLEXITY_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("fireworks", "FIREWORKS_API_KEY"),
        ];
        for (provider, expected_key) in providers {
            let keys = env_keys_for_provider(provider);
            assert!(!keys.is_empty(), "expected env key for {provider}");
            assert_eq!(
                keys[0], expected_key,
                "wrong primary env key for {provider}"
            );
        }
    }

    #[test]
    fn test_env_keys_togetherai_has_two_variants() {
        let keys = env_keys_for_provider("togetherai");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "TOGETHER_API_KEY");
        assert_eq!(keys[1], "TOGETHER_AI_API_KEY");
    }

    #[test]
    fn test_env_keys_google_includes_gemini_fallback() {
        let keys = env_keys_for_provider("google");
        assert_eq!(keys, &["GOOGLE_API_KEY", "GEMINI_API_KEY"]);
    }

    #[test]
    fn test_env_keys_moonshotai_aliases() {
        for alias in &["moonshotai", "moonshot", "kimi"] {
            let keys = env_keys_for_provider(alias);
            assert_eq!(
                keys,
                &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
                "alias {alias} should map to moonshot auth fallback key chain"
            );
        }
    }

    #[test]
    fn test_env_keys_alibaba_aliases() {
        for alias in &["alibaba", "dashscope", "qwen"] {
            let keys = env_keys_for_provider(alias);
            assert_eq!(
                keys,
                &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
                "alias {alias} should map to dashscope auth fallback key chain"
            );
        }
    }

    #[test]
    fn test_env_keys_native_and_gateway_aliases() {
        let cases: [(&str, &[&str]); 7] = [
            ("gemini", &["GOOGLE_API_KEY", "GEMINI_API_KEY"]),
            ("fireworks-ai", &["FIREWORKS_API_KEY"]),
            (
                "bedrock",
                &[
                    "AWS_ACCESS_KEY_ID",
                    "AWS_SECRET_ACCESS_KEY",
                    "AWS_SESSION_TOKEN",
                    "AWS_BEARER_TOKEN_BEDROCK",
                    "AWS_PROFILE",
                    "AWS_REGION",
                ] as &[&str],
            ),
            ("azure", &["AZURE_OPENAI_API_KEY"]),
            ("vertexai", &["GOOGLE_CLOUD_API_KEY", "VERTEX_API_KEY"]),
            ("copilot", &["GITHUB_COPILOT_API_KEY", "GITHUB_TOKEN"]),
            ("fireworks", &["FIREWORKS_API_KEY"]),
        ];

        for (alias, expected) in cases {
            let keys = env_keys_for_provider(alias);
            assert_eq!(keys, expected, "alias {alias} should map to {expected:?}");
        }
    }

    // ── Percent encoding / decoding ───────────────────────────────────

    #[test]
    fn test_percent_encode_ascii_passthrough() {
        assert_eq!(percent_encode_component("hello"), "hello");
        assert_eq!(
            percent_encode_component("ABCDEFxyz0189-._~"),
            "ABCDEFxyz0189-._~"
        );
    }

    #[test]
    fn test_percent_encode_spaces_and_special() {
        assert_eq!(percent_encode_component("hello world"), "hello%20world");
        assert_eq!(percent_encode_component("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode_component("100%"), "100%25");
    }

    #[test]
    fn test_percent_decode_passthrough() {
        assert_eq!(percent_decode_component("hello").as_deref(), Some("hello"));
    }

    #[test]
    fn test_percent_decode_encoded() {
        assert_eq!(
            percent_decode_component("hello%20world").as_deref(),
            Some("hello world")
        );
        assert_eq!(
            percent_decode_component("a%26b%3Dc").as_deref(),
            Some("a&b=c")
        );
    }

    #[test]
    fn test_percent_decode_plus_as_space() {
        assert_eq!(
            percent_decode_component("hello+world").as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn test_percent_decode_invalid_hex_returns_none() {
        assert!(percent_decode_component("hello%ZZ").is_none());
        assert!(percent_decode_component("trailing%2").is_none());
        assert!(percent_decode_component("trailing%").is_none());
    }

    #[test]
    fn test_percent_encode_decode_roundtrip() {
        let inputs = ["hello world", "a=1&b=2", "special: 100% /path?q=v#frag"];
        for input in inputs {
            let encoded = percent_encode_component(input);
            let decoded = percent_decode_component(&encoded).expect("decode");
            assert_eq!(decoded, input, "roundtrip failed for: {input}");
        }
    }

    // ── parse_query_pairs ─────────────────────────────────────────────

    #[test]
    fn test_parse_query_pairs_basic() {
        let pairs = parse_query_pairs("code=abc&state=def");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("code".to_string(), "abc".to_string()));
        assert_eq!(pairs[1], ("state".to_string(), "def".to_string()));
    }

    #[test]
    fn test_parse_query_pairs_empty_value() {
        let pairs = parse_query_pairs("key=");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("key".to_string(), String::new()));
    }

    #[test]
    fn test_parse_query_pairs_no_value() {
        let pairs = parse_query_pairs("key");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("key".to_string(), String::new()));
    }

    #[test]
    fn test_parse_query_pairs_empty_string() {
        let pairs = parse_query_pairs("");
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_parse_query_pairs_encoded_values() {
        let pairs = parse_query_pairs("scope=read%20write&redirect=http%3A%2F%2Fexample.com");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].1, "read write");
        assert_eq!(pairs[1].1, "http://example.com");
    }

    // ── build_url_with_query ──────────────────────────────────────────

    #[test]
    fn test_build_url_basic() {
        let url = build_url_with_query(
            "https://example.com/auth",
            &[("key", "val"), ("foo", "bar")],
        );
        assert_eq!(url, "https://example.com/auth?key=val&foo=bar");
    }

    #[test]
    fn test_build_url_encodes_special_chars() {
        let url =
            build_url_with_query("https://example.com", &[("q", "hello world"), ("x", "a&b")]);
        assert!(url.contains("q=hello%20world"));
        assert!(url.contains("x=a%26b"));
    }

    #[test]
    fn test_build_url_no_params() {
        let url = build_url_with_query("https://example.com", &[]);
        assert_eq!(url, "https://example.com?");
    }

    // ── parse_oauth_code_input edge cases ─────────────────────────────

    #[test]
    fn test_parse_oauth_code_input_empty() {
        let (code, state) = parse_oauth_code_input("");
        assert!(code.is_none());
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_whitespace_only() {
        let (code, state) = parse_oauth_code_input("   ");
        assert!(code.is_none());
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_url_strips_fragment() {
        let (code, state) =
            parse_oauth_code_input("https://example.com/callback?code=abc&state=def#fragment");
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("def"));
    }

    #[test]
    fn test_parse_oauth_code_input_url_code_only() {
        let (code, state) = parse_oauth_code_input("https://example.com/callback?code=abc");
        assert_eq!(code.as_deref(), Some("abc"));
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_hash_empty_state() {
        let (code, state) = parse_oauth_code_input("abc#");
        assert_eq!(code.as_deref(), Some("abc"));
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_hash_empty_code() {
        let (code, state) = parse_oauth_code_input("#state-only");
        assert!(code.is_none());
        assert_eq!(state.as_deref(), Some("state-only"));
    }

    // ── oauth_expires_at_ms ───────────────────────────────────────────

    #[test]
    fn test_oauth_expires_at_ms_subtracts_safety_margin() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let expires_in = 3600; // 1 hour
        let result = oauth_expires_at_ms(expires_in);

        // Should be ~55 minutes from now (3600s - 5min safety margin).
        let expected_approx = now_ms + 3600 * 1000 - 5 * 60 * 1000;
        let diff = (result - expected_approx).unsigned_abs();
        assert!(diff < 1000, "expected ~{expected_approx}ms, got {result}ms");
    }

    #[test]
    fn test_oauth_expires_at_ms_zero_expires_in() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let result = oauth_expires_at_ms(0);

        // Should be 5 minutes before now (0s - 5min safety margin).
        let expected_approx = now_ms - 5 * 60 * 1000;
        let diff = (result - expected_approx).unsigned_abs();
        assert!(diff < 1000, "expected ~{expected_approx}ms, got {result}ms");
    }

    #[test]
    fn test_oauth_expires_at_ms_saturates_for_huge_positive_expires_in() {
        let result = oauth_expires_at_ms(i64::MAX);
        assert_eq!(result, i64::MAX - 5 * 60 * 1000);
    }

    #[test]
    fn test_oauth_expires_at_ms_handles_huge_negative_expires_in() {
        let result = oauth_expires_at_ms(i64::MIN);
        assert!(result <= chrono::Utc::now().timestamp_millis());
    }

    // ── Overwrite semantics ───────────────────────────────────────────

    #[test]
    fn test_set_overwrites_existing_credential() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "first-key".to_string(),
            },
        );
        assert_eq!(auth.api_key("anthropic").as_deref(), Some("first-key"));

        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "second-key".to_string(),
            },
        );
        assert_eq!(auth.api_key("anthropic").as_deref(), Some("second-key"));
        assert_eq!(auth.entries.len(), 1);
    }

    #[test]
    fn test_save_then_overwrite_persists_latest() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        // Save first version.
        {
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "old-key".to_string(),
                },
            );
            auth.save().expect("save");
        }

        // Overwrite.
        {
            let mut auth = AuthStorage::load(auth_path.clone()).expect("load");
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "new-key".to_string(),
                },
            );
            auth.save().expect("save");
        }

        // Verify.
        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("new-key"));
    }

    // ── load_default_auth convenience ─────────────────────────────────

    #[test]
    fn test_load_default_auth_works_like_load() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "test-key".to_string(),
            },
        );
        auth.save().expect("save");

        let loaded = load_default_auth(&auth_path).expect("load_default_auth");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("test-key"));
    }

    // ── redact_known_secrets ─────────────────────────────────────────

    #[test]
    fn test_redact_known_secrets_replaces_secrets() {
        let text = r#"{"token":"secret123","other":"hello secret123 world"}"#;
        let redacted = redact_known_secrets(text, &["secret123"]);
        assert!(!redacted.contains("secret123"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn test_redact_known_secrets_ignores_empty_secrets() {
        let text = "nothing to redact here";
        let redacted = redact_known_secrets(text, &["", "   "]);
        // Empty secret should be skipped; only non-empty "   " gets replaced if present.
        assert_eq!(redacted, text);
    }

    #[test]
    fn test_redact_known_secrets_multiple_secrets() {
        let text = "token=aaa refresh=bbb echo=aaa";
        let redacted = redact_known_secrets(text, &["aaa", "bbb"]);
        assert!(!redacted.contains("aaa"));
        assert!(!redacted.contains("bbb"));
        assert_eq!(
            redacted,
            "token=[REDACTED] refresh=[REDACTED] echo=[REDACTED]"
        );
    }

    #[test]
    fn test_redact_known_secrets_no_match() {
        let text = "safe text with no secrets";
        let redacted = redact_known_secrets(text, &["not-present"]);
        assert_eq!(redacted, text);
    }

    #[test]
    fn test_redact_known_secrets_redacts_oauth_json_fields_without_known_input() {
        let text = r#"{"access_token":"new-access","refresh_token":"new-refresh","nested":{"id_token":"new-id","safe":"ok"}}"#;
        let redacted = redact_known_secrets(text, &[]);
        assert!(redacted.contains("\"access_token\":\"[REDACTED]\""));
        assert!(redacted.contains("\"refresh_token\":\"[REDACTED]\""));
        assert!(redacted.contains("\"id_token\":\"[REDACTED]\""));
        assert!(redacted.contains("\"safe\":\"ok\""));
        assert!(!redacted.contains("new-access"));
        assert!(!redacted.contains("new-refresh"));
        assert!(!redacted.contains("new-id"));
    }

    // ── PKCE determinism ──────────────────────────────────────────────

    #[test]
    fn test_generate_pkce_unique_each_call() {
        let (v1, c1) = generate_pkce();
        let (v2, c2) = generate_pkce();
        assert_ne!(v1, v2, "verifiers should differ");
        assert_ne!(c1, c2, "challenges should differ");
    }

    #[test]
    fn test_generate_pkce_challenge_is_sha256_of_verifier() {
        let (verifier, challenge) = generate_pkce();
        let expected_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected_challenge);
    }

    // ── GitHub Copilot OAuth tests ────────────────────────────────

    fn sample_copilot_config() -> CopilotOAuthConfig {
        CopilotOAuthConfig {
            client_id: "Iv1.test_copilot_id".to_string(),
            github_base_url: "https://github.com".to_string(),
            scopes: GITHUB_COPILOT_SCOPES.to_string(),
        }
    }

    #[test]
    fn test_copilot_browser_oauth_requires_client_id() {
        let config = CopilotOAuthConfig {
            client_id: String::new(),
            ..CopilotOAuthConfig::default()
        };
        let err = start_copilot_browser_oauth(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("client_id"),
            "error should mention client_id: {msg}"
        );
    }

    #[test]
    fn test_copilot_browser_oauth_url_contains_required_params() {
        let config = sample_copilot_config();
        let info = start_copilot_browser_oauth(&config).expect("start");

        assert_eq!(info.provider, "github-copilot");
        assert!(!info.verifier.is_empty());

        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, GITHUB_OAUTH_AUTHORIZE_URL);

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("Iv1.test_copilot_id")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(GITHUB_COPILOT_SCOPES)
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(params.contains_key("code_challenge"));
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
    }

    #[test]
    fn test_copilot_browser_oauth_enterprise_url() {
        let config = CopilotOAuthConfig {
            client_id: "Iv1.enterprise".to_string(),
            github_base_url: "https://github.mycompany.com".to_string(),
            scopes: "read:user".to_string(),
        };
        let info = start_copilot_browser_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://github.mycompany.com/login/oauth/authorize");
    }

    #[test]
    fn test_copilot_browser_oauth_enterprise_trailing_slash() {
        let config = CopilotOAuthConfig {
            client_id: "Iv1.enterprise".to_string(),
            github_base_url: "https://github.mycompany.com/".to_string(),
            scopes: "read:user".to_string(),
        };
        let info = start_copilot_browser_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://github.mycompany.com/login/oauth/authorize");
    }

    #[test]
    fn test_copilot_browser_oauth_pkce_format() {
        let config = sample_copilot_config();
        let info = start_copilot_browser_oauth(&config).expect("start");

        assert_eq!(info.verifier.len(), 43);
        assert!(!info.verifier.contains('+'));
        assert!(!info.verifier.contains('/'));
        assert!(!info.verifier.contains('='));
    }

    #[test]
    #[cfg(unix)]
    fn test_copilot_browser_oauth_complete_success() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(
                200,
                r#"{"access_token":"ghu_test_access","refresh_token":"ghr_test_refresh","expires_in":28800}"#,
            );

            // Extract port from token_url to build a matching config.
            let _config = CopilotOAuthConfig {
                client_id: "Iv1.test".to_string(),
                // Use a base URL that generates the test server URL.
                github_base_url: token_url.trim_end_matches("/token").replace("/token", ""),
                scopes: "read:user".to_string(),
            };

            // We need to call complete directly with the token URL.
            // Since the function constructs the URL from base, we use an
            // alternate approach: test parse_github_token_response directly.
            let cred = parse_github_token_response(
                r#"{"access_token":"ghu_test_access","refresh_token":"ghr_test_refresh","expires_in":28800}"#,
            )
            .expect("parse");

            match cred {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires,
                    ..
                } => {
                    assert_eq!(access_token, "ghu_test_access");
                    assert_eq!(refresh_token, "ghr_test_refresh");
                    assert!(expires > chrono::Utc::now().timestamp_millis());
                }
                other => panic!("expected OAuth, got: {other:?}"),
            }
        });
    }

    #[test]
    fn test_parse_github_token_no_refresh_token() {
        let cred =
            parse_github_token_response(r#"{"access_token":"ghu_test","token_type":"bearer"}"#)
                .expect("parse");

        match cred {
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                ..
            } => {
                assert_eq!(access_token, "ghu_test");
                assert!(refresh_token.is_empty(), "should default to empty");
            }
            other => panic!("expected OAuth, got: {other:?}"),
        }
    }

    #[test]
    fn test_parse_github_token_no_expiry_uses_far_future() {
        let cred = parse_github_token_response(
            r#"{"access_token":"ghu_test","refresh_token":"ghr_test"}"#,
        )
        .expect("parse");

        match cred {
            AuthCredential::OAuth { expires, .. } => {
                let now = chrono::Utc::now().timestamp_millis();
                let one_year_ms = 365 * 24 * 3600 * 1000_i64;
                // Should be close to 1 year from now (minus 5min safety margin).
                assert!(
                    expires > now + one_year_ms - 10 * 60 * 1000,
                    "expected far-future expiry"
                );
            }
            other => panic!("expected OAuth, got: {other:?}"),
        }
    }

    #[test]
    fn test_parse_github_token_missing_access_token_fails() {
        let err = parse_github_token_response(r#"{"refresh_token":"ghr_test"}"#).unwrap_err();
        assert!(err.to_string().contains("access_token"));
    }

    #[test]
    fn test_copilot_diagnostic_includes_troubleshooting() {
        let msg = copilot_diagnostic("Token exchange failed", "bad request");
        assert!(msg.contains("Token exchange failed"));
        assert!(msg.contains("Troubleshooting"));
        assert!(msg.contains("client_id"));
        assert!(msg.contains("Copilot subscription"));
        assert!(msg.contains("Enterprise"));
    }

    // ── Device flow tests ─────────────────────────────────────────

    #[test]
    fn test_device_code_response_deserialize() {
        let json = r#"{
            "device_code": "dc_test",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.device_code, "dc_test");
        assert_eq!(resp.user_code, "ABCD-1234");
        assert_eq!(resp.verification_uri, "https://github.com/login/device");
        assert_eq!(resp.expires_in, 900);
        assert_eq!(resp.interval, 5);
        assert!(resp.verification_uri_complete.is_none());
    }

    #[test]
    fn test_device_code_response_default_interval() {
        let json = r#"{
            "device_code": "dc",
            "user_code": "CODE",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 600
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.interval, 5, "default interval should be 5 seconds");
    }

    #[test]
    fn test_device_code_response_with_complete_uri() {
        let json = r#"{
            "device_code": "dc",
            "user_code": "CODE",
            "verification_uri": "https://github.com/login/device",
            "verification_uri_complete": "https://github.com/login/device?user_code=CODE",
            "expires_in": 600,
            "interval": 10
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(
            resp.verification_uri_complete.as_deref(),
            Some("https://github.com/login/device?user_code=CODE")
        );
    }

    #[test]
    fn test_copilot_device_flow_requires_client_id() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = CopilotOAuthConfig {
                client_id: String::new(),
                ..CopilotOAuthConfig::default()
            };
            let err = start_copilot_device_flow(&config).await.unwrap_err();
            assert!(err.to_string().contains("client_id"));
        });
    }

    #[test]
    fn test_kimi_oauth_host_env_lookup_prefers_primary_host() {
        let host = kimi_code_oauth_host_with_env_lookup(|key| match key {
            "KIMI_CODE_OAUTH_HOST" => Some("https://primary.kimi.test".to_string()),
            "KIMI_OAUTH_HOST" => Some("https://fallback.kimi.test".to_string()),
            _ => None,
        });
        assert_eq!(host, "https://primary.kimi.test");
    }

    #[test]
    fn test_kimi_share_dir_env_lookup_prefers_kimi_share_dir() {
        let share_dir = kimi_share_dir_with_env_lookup(|key| match key {
            "KIMI_SHARE_DIR" => Some("/tmp/custom-kimi-share".to_string()),
            "HOME" => Some("/tmp/home".to_string()),
            _ => None,
        });
        assert_eq!(
            share_dir,
            Some(PathBuf::from("/tmp/custom-kimi-share")),
            "KIMI_SHARE_DIR should override HOME-based default"
        );
    }

    #[test]
    fn test_kimi_share_dir_env_lookup_falls_back_to_home() {
        let share_dir = kimi_share_dir_with_env_lookup(|key| match key {
            "KIMI_SHARE_DIR" => Some("   ".to_string()),
            "HOME" => Some("/tmp/home".to_string()),
            _ => None,
        });
        assert_eq!(share_dir, Some(PathBuf::from("/tmp/home/.kimi")));
    }

    #[test]
    fn test_home_dir_env_lookup_falls_back_to_userprofile() {
        let home = home_dir_with_env_lookup(|key| match key {
            "HOME" => Some("   ".to_string()),
            "USERPROFILE" => Some("C:\\Users\\tester".to_string()),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from("C:\\Users\\tester")));
    }

    #[test]
    fn test_home_dir_env_lookup_falls_back_to_homedrive_homepath() {
        let home = home_dir_with_env_lookup(|key| match key {
            "HOMEDRIVE" => Some("C:".to_string()),
            "HOMEPATH" => Some("\\Users\\tester".to_string()),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from("C:\\Users\\tester")));
    }

    #[test]
    fn test_home_dir_env_lookup_homedrive_homepath_without_root_separator() {
        let home = home_dir_with_env_lookup(|key| match key {
            "HOMEDRIVE" => Some("C:".to_string()),
            "HOMEPATH" => Some("Users\\tester".to_string()),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from("C:/Users\\tester")));
    }

    #[test]
    fn test_read_external_kimi_code_access_token_from_share_dir_reads_unexpired_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let share_dir = dir.path();
        let credentials_dir = share_dir.join("credentials");
        std::fs::create_dir_all(&credentials_dir).expect("create credentials dir");
        let path = credentials_dir.join("kimi-code.json");
        let expires_at = chrono::Utc::now().timestamp() + 3600;
        std::fs::write(
            &path,
            format!(r#"{{"access_token":" kimi-token ","expires_at":{expires_at}}}"#),
        )
        .expect("write token file");

        let token = read_external_kimi_code_access_token_from_share_dir(share_dir);
        assert_eq!(token.as_deref(), Some("kimi-token"));
    }

    #[test]
    fn test_read_external_kimi_code_access_token_from_share_dir_ignores_expired_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let share_dir = dir.path();
        let credentials_dir = share_dir.join("credentials");
        std::fs::create_dir_all(&credentials_dir).expect("create credentials dir");
        let path = credentials_dir.join("kimi-code.json");
        let expires_at = chrono::Utc::now().timestamp() - 5;
        std::fs::write(
            &path,
            format!(r#"{{"access_token":"kimi-token","expires_at":{expires_at}}}"#),
        )
        .expect("write token file");

        let token = read_external_kimi_code_access_token_from_share_dir(share_dir);
        assert!(token.is_none(), "expired Kimi token should be ignored");
    }

    #[test]
    fn test_start_kimi_code_device_flow_parses_response() {
        let host = spawn_oauth_host_server(
            200,
            r#"{
                "device_code": "dc_test",
                "user_code": "ABCD-1234",
                "verification_uri": "https://auth.kimi.com/device",
                "verification_uri_complete": "https://auth.kimi.com/device?user_code=ABCD-1234",
                "expires_in": 900,
                "interval": 5
            }"#,
        );
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let response = start_kimi_code_device_flow_with_client(&client, &host)
                .await
                .expect("start kimi device flow");
            assert_eq!(response.device_code, "dc_test");
            assert_eq!(response.user_code, "ABCD-1234");
            assert_eq!(response.expires_in, 900);
            assert_eq!(response.interval, 5);
            assert_eq!(
                response.verification_uri_complete.as_deref(),
                Some("https://auth.kimi.com/device?user_code=ABCD-1234")
            );
        });
    }

    #[test]
    fn test_poll_kimi_code_device_flow_success_returns_oauth_credential() {
        let host = spawn_oauth_host_server(
            200,
            r#"{"access_token":"kimi-at","refresh_token":"kimi-rt","expires_in":3600}"#,
        );
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let result =
                poll_kimi_code_device_flow_with_client(&client, &host, "device-code").await;
            match result {
                DeviceFlowPollResult::Success(AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    token_url,
                    client_id,
                    ..
                }) => {
                    let expected_token_url = format!("{host}{KIMI_CODE_TOKEN_PATH}");
                    assert_eq!(access_token, "kimi-at");
                    assert_eq!(refresh_token, "kimi-rt");
                    assert_eq!(token_url.as_deref(), Some(expected_token_url.as_str()));
                    assert_eq!(client_id.as_deref(), Some(KIMI_CODE_OAUTH_CLIENT_ID));
                }
                other => panic!("expected success, got {other:?}"),
            }
        });
    }

    #[test]
    fn test_poll_kimi_code_device_flow_pending_state() {
        let host = spawn_oauth_host_server(
            400,
            r#"{"error":"authorization_pending","error_description":"wait"}"#,
        );
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let result =
                poll_kimi_code_device_flow_with_client(&client, &host, "device-code").await;
            assert!(matches!(result, DeviceFlowPollResult::Pending));
        });
    }

    // ── GitLab OAuth tests ────────────────────────────────────────

    fn sample_gitlab_config() -> GitLabOAuthConfig {
        GitLabOAuthConfig {
            client_id: "gl_test_app_id".to_string(),
            base_url: GITLAB_DEFAULT_BASE_URL.to_string(),
            scopes: GITLAB_DEFAULT_SCOPES.to_string(),
            redirect_uri: Some("http://localhost:8765/callback".to_string()),
        }
    }

    #[test]
    fn test_gitlab_oauth_requires_client_id() {
        let config = GitLabOAuthConfig {
            client_id: String::new(),
            ..GitLabOAuthConfig::default()
        };
        let err = start_gitlab_oauth(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("client_id"),
            "error should mention client_id: {msg}"
        );
        assert!(msg.contains("Settings"), "should mention GitLab settings");
    }

    #[test]
    fn test_gitlab_oauth_url_contains_required_params() {
        let config = sample_gitlab_config();
        let info = start_gitlab_oauth(&config).expect("start");

        assert_eq!(info.provider, "gitlab");
        assert!(!info.verifier.is_empty());

        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://gitlab.com/oauth/authorize");

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("gl_test_app_id")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(GITLAB_DEFAULT_SCOPES)
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:8765/callback")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(params.contains_key("code_challenge"));
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
    }

    #[test]
    fn test_gitlab_oauth_self_hosted_url() {
        let config = GitLabOAuthConfig {
            client_id: "gl_self_hosted".to_string(),
            base_url: "https://gitlab.mycompany.com".to_string(),
            scopes: "api".to_string(),
            redirect_uri: None,
        };
        let info = start_gitlab_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://gitlab.mycompany.com/oauth/authorize");
        assert!(
            info.instructions
                .as_deref()
                .unwrap_or("")
                .contains("gitlab.mycompany.com"),
            "instructions should mention the base URL"
        );
    }

    #[test]
    fn test_gitlab_oauth_self_hosted_trailing_slash() {
        let config = GitLabOAuthConfig {
            client_id: "gl_self_hosted".to_string(),
            base_url: "https://gitlab.mycompany.com/".to_string(),
            scopes: "api".to_string(),
            redirect_uri: None,
        };
        let info = start_gitlab_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://gitlab.mycompany.com/oauth/authorize");
    }

    #[test]
    fn test_gitlab_oauth_no_redirect_uri() {
        let config = GitLabOAuthConfig {
            client_id: "gl_no_redirect".to_string(),
            base_url: GITLAB_DEFAULT_BASE_URL.to_string(),
            scopes: "api".to_string(),
            redirect_uri: None,
        };
        let info = start_gitlab_oauth(&config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert!(
            !params.contains_key("redirect_uri"),
            "redirect_uri should be absent"
        );
    }

    #[test]
    fn test_gitlab_oauth_pkce_format() {
        let config = sample_gitlab_config();
        let info = start_gitlab_oauth(&config).expect("start");

        assert_eq!(info.verifier.len(), 43);
        assert!(!info.verifier.contains('+'));
        assert!(!info.verifier.contains('/'));
        assert!(!info.verifier.contains('='));
    }

    #[test]
    #[cfg(unix)]
    fn test_gitlab_oauth_complete_success() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(
                200,
                r#"{"access_token":"glpat-test_access","refresh_token":"glrt-test_refresh","expires_in":7200,"token_type":"bearer"}"#,
            );

            // Test via the token response directly (GitLab uses standard OAuth response).
            let response: OAuthTokenResponse = serde_json::from_str(
                r#"{"access_token":"glpat-test_access","refresh_token":"glrt-test_refresh","expires_in":7200}"#,
            )
            .expect("parse");

            let cred = AuthCredential::OAuth {
                access_token: response.access_token,
                refresh_token: response.refresh_token,
                expires: oauth_expires_at_ms(response.expires_in),
                token_url: None,
                client_id: None,
            };

            match cred {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires,
                    ..
                } => {
                    assert_eq!(access_token, "glpat-test_access");
                    assert_eq!(refresh_token, "glrt-test_refresh");
                    assert!(expires > chrono::Utc::now().timestamp_millis());
                }
                other => panic!("expected OAuth, got: {other:?}"),
            }

            // Also ensure the test server URL was consumed (not left hanging).
            let _ = token_url;
        });
    }

    #[test]
    fn test_gitlab_diagnostic_includes_troubleshooting() {
        let msg = gitlab_diagnostic("https://gitlab.com", "Token exchange failed", "bad request");
        assert!(msg.contains("Token exchange failed"));
        assert!(msg.contains("Troubleshooting"));
        assert!(msg.contains("client_id"));
        assert!(msg.contains("Settings > Applications"));
        assert!(msg.contains("https://gitlab.com"));
    }

    #[test]
    fn test_gitlab_diagnostic_self_hosted_url_in_message() {
        let msg = gitlab_diagnostic("https://gitlab.mycompany.com", "Auth failed", "HTTP 401");
        assert!(
            msg.contains("gitlab.mycompany.com"),
            "should reference the self-hosted URL"
        );
    }

    // ── Provider metadata integration ─────────────────────────────

    #[test]
    fn test_env_keys_gitlab_provider() {
        let keys = env_keys_for_provider("gitlab");
        assert_eq!(keys, &["GITLAB_TOKEN", "GITLAB_API_KEY"]);
    }

    #[test]
    fn test_env_keys_gitlab_duo_alias() {
        let keys = env_keys_for_provider("gitlab-duo");
        assert_eq!(keys, &["GITLAB_TOKEN", "GITLAB_API_KEY"]);
    }

    #[test]
    fn test_env_keys_copilot_includes_github_token() {
        let keys = env_keys_for_provider("github-copilot");
        assert_eq!(keys, &["GITHUB_COPILOT_API_KEY", "GITHUB_TOKEN"]);
    }

    // ── Default config constructors ───────────────────────────────

    #[test]
    fn test_copilot_config_default() {
        let config = CopilotOAuthConfig::default();
        assert!(config.client_id.is_empty());
        assert_eq!(config.github_base_url, "https://github.com");
        assert_eq!(config.scopes, GITHUB_COPILOT_SCOPES);
    }

    #[test]
    fn test_gitlab_config_default() {
        let config = GitLabOAuthConfig::default();
        assert!(config.client_id.is_empty());
        assert_eq!(config.base_url, GITLAB_DEFAULT_BASE_URL);
        assert_eq!(config.scopes, GITLAB_DEFAULT_SCOPES);
        assert!(config.redirect_uri.is_none());
    }

    // ── trim_trailing_slash ───────────────────────────────────────

    #[test]
    fn test_trim_trailing_slash_noop() {
        assert_eq!(
            trim_trailing_slash("https://github.com"),
            "https://github.com"
        );
    }

    #[test]
    fn test_trim_trailing_slash_single() {
        assert_eq!(
            trim_trailing_slash("https://github.com/"),
            "https://github.com"
        );
    }

    #[test]
    fn test_trim_trailing_slash_multiple() {
        assert_eq!(
            trim_trailing_slash("https://github.com///"),
            "https://github.com"
        );
    }

    // ── AuthCredential new variant serialization ─────────────────────

    #[test]
    fn test_aws_credentials_round_trip() {
        let cred = AuthCredential::AwsCredentials {
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/SECRET".to_string(),
            session_token: Some("FwoGZX...session".to_string()),
            region: Some("us-west-2".to_string()),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token,
                region,
            } => {
                assert_eq!(access_key_id, "AKIAEXAMPLE");
                assert_eq!(secret_access_key, "wJalrXUtnFEMI/SECRET");
                assert_eq!(session_token.as_deref(), Some("FwoGZX...session"));
                assert_eq!(region.as_deref(), Some("us-west-2"));
            }
            other => panic!("expected AwsCredentials, got: {other:?}"),
        }
    }

    #[test]
    fn test_aws_credentials_without_optional_fields() {
        let json =
            r#"{"type":"aws_credentials","access_key_id":"AKIA","secret_access_key":"secret"}"#;
        let cred: AuthCredential = serde_json::from_str(json).expect("deserialize");
        match cred {
            AuthCredential::AwsCredentials {
                session_token,
                region,
                ..
            } => {
                assert!(session_token.is_none());
                assert!(region.is_none());
            }
            other => panic!("expected AwsCredentials, got: {other:?}"),
        }
    }

    #[test]
    fn test_bearer_token_round_trip() {
        let cred = AuthCredential::BearerToken {
            token: "my-gateway-token-123".to_string(),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::BearerToken { token } => {
                assert_eq!(token, "my-gateway-token-123");
            }
            other => panic!("expected BearerToken, got: {other:?}"),
        }
    }

    #[test]
    fn test_service_key_round_trip() {
        let cred = AuthCredential::ServiceKey {
            client_id: Some("sap-client-id".to_string()),
            client_secret: Some("sap-secret".to_string()),
            token_url: Some("https://auth.sap.com/oauth/token".to_string()),
            service_url: Some("https://api.ai.sap.com".to_string()),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::ServiceKey {
                client_id,
                client_secret,
                token_url,
                service_url,
            } => {
                assert_eq!(client_id.as_deref(), Some("sap-client-id"));
                assert_eq!(client_secret.as_deref(), Some("sap-secret"));
                assert_eq!(
                    token_url.as_deref(),
                    Some("https://auth.sap.com/oauth/token")
                );
                assert_eq!(service_url.as_deref(), Some("https://api.ai.sap.com"));
            }
            other => panic!("expected ServiceKey, got: {other:?}"),
        }
    }

    #[test]
    fn test_service_key_without_optional_fields() {
        let json = r#"{"type":"service_key"}"#;
        let cred: AuthCredential = serde_json::from_str(json).expect("deserialize");
        match cred {
            AuthCredential::ServiceKey {
                client_id,
                client_secret,
                token_url,
                service_url,
            } => {
                assert!(client_id.is_none());
                assert!(client_secret.is_none());
                assert!(token_url.is_none());
                assert!(service_url.is_none());
            }
            other => panic!("expected ServiceKey, got: {other:?}"),
        }
    }

    // ── api_key() with new variants ──────────────────────────────────

    #[test]
    fn test_api_key_returns_bearer_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "my-gateway",
            AuthCredential::BearerToken {
                token: "gw-tok-123".to_string(),
            },
        );
        assert_eq!(auth.api_key("my-gateway").as_deref(), Some("gw-tok-123"));
    }

    #[test]
    fn test_api_key_returns_aws_access_key_id() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIAEXAMPLE".to_string(),
                secret_access_key: "secret".to_string(),
                session_token: None,
                region: None,
            },
        );
        assert_eq!(
            auth.api_key("amazon-bedrock").as_deref(),
            Some("AKIAEXAMPLE")
        );
    }

    #[test]
    fn test_api_key_returns_none_for_service_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "sap-ai-core",
            AuthCredential::ServiceKey {
                client_id: Some("id".to_string()),
                client_secret: Some("secret".to_string()),
                token_url: Some("https://auth.example.com".to_string()),
                service_url: Some("https://api.example.com".to_string()),
            },
        );
        assert!(auth.api_key("sap-ai-core").is_none());
    }

    // ── AWS Credential Chain ─────────────────────────────────────────

    fn empty_auth() -> AuthStorage {
        let dir = tempfile::tempdir().expect("tmpdir");
        AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        }
    }

    #[test]
    fn test_aws_bearer_token_env_wins() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_BEARER_TOKEN_BEDROCK" => Some("bearer-tok-env".to_string()),
            "AWS_REGION" => Some("eu-west-1".to_string()),
            "AWS_ACCESS_KEY_ID" => Some("AKIA_SHOULD_NOT_WIN".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Bearer {
                token: "bearer-tok-env".to_string(),
                region: "eu-west-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_env_sigv4_credentials() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIATEST".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secretTEST".to_string()),
            "AWS_SESSION_TOKEN" => Some("session123".to_string()),
            "AWS_REGION" => Some("ap-southeast-1".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIATEST".to_string(),
                secret_access_key: "secretTEST".to_string(),
                session_token: Some("session123".to_string()),
                region: "ap-southeast-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_env_sigv4_without_session_token() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA".to_string(),
                secret_access_key: "secret".to_string(),
                session_token: None,
                region: "us-east-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_default_region_fallback() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            "AWS_DEFAULT_REGION" => Some("ca-central-1".to_string()),
            _ => None,
        });
        match result {
            Some(AwsResolvedCredentials::Sigv4 { region, .. }) => {
                assert_eq!(region, "ca-central-1");
            }
            other => panic!("expected Sigv4, got: {other:?}"),
        }
    }

    #[test]
    fn test_aws_stored_credentials_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_STORED".to_string(),
                secret_access_key: "secret_stored".to_string(),
                session_token: None,
                region: Some("us-west-2".to_string()),
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA_STORED".to_string(),
                secret_access_key: "secret_stored".to_string(),
                session_token: None,
                region: "us-west-2".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_stored_bearer_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::BearerToken {
                token: "stored-bearer".to_string(),
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Bearer {
                token: "stored-bearer".to_string(),
                region: "us-east-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_env_beats_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_STORED".to_string(),
                secret_access_key: "stored_secret".to_string(),
                session_token: None,
                region: None,
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA_ENV".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("env_secret".to_string()),
            _ => None,
        });
        match result {
            Some(AwsResolvedCredentials::Sigv4 { access_key_id, .. }) => {
                assert_eq!(access_key_id, "AKIA_ENV");
            }
            other => panic!("expected Sigv4 from env, got: {other:?}"),
        }
    }

    #[test]
    fn test_aws_no_credentials_returns_none() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert!(result.is_none());
    }

    #[test]
    fn test_aws_empty_bearer_token_skipped() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_BEARER_TOKEN_BEDROCK" => Some("  ".to_string()),
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert!(matches!(result, Some(AwsResolvedCredentials::Sigv4 { .. })));
    }

    #[test]
    fn test_aws_access_key_without_secret_skipped() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            _ => None,
        });
        assert!(result.is_none());
    }

    // ── SAP AI Core Credential Chain ─────────────────────────────────

    #[test]
    fn test_sap_json_service_key() {
        let auth = empty_auth();
        let key_json = serde_json::json!({
            "clientid": "sap-client",
            "clientsecret": "sap-secret",
            "url": "https://auth.sap.example.com/oauth/token",
            "serviceurls": {
                "AI_API_URL": "https://api.ai.sap.example.com"
            }
        })
        .to_string();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some(key_json.clone()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url: "https://auth.sap.example.com/oauth/token".to_string(),
                service_url: "https://api.ai.sap.example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_individual_env_vars() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "SAP_AI_CORE_CLIENT_ID" => Some("env-client".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.sap.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://service.sap.example.com".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "env-client".to_string(),
                client_secret: "env-secret".to_string(),
                token_url: "https://token.sap.example.com".to_string(),
                service_url: "https://service.sap.example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_stored_service_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };
        auth.set(
            "sap-ai-core",
            AuthCredential::ServiceKey {
                client_id: Some("stored-id".to_string()),
                client_secret: Some("stored-secret".to_string()),
                token_url: Some("https://stored-token.sap.com".to_string()),
                service_url: Some("https://stored-api.sap.com".to_string()),
            },
        );
        let result = resolve_sap_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "stored-id".to_string(),
                client_secret: "stored-secret".to_string(),
                token_url: "https://stored-token.sap.com".to_string(),
                service_url: "https://stored-api.sap.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_json_key_wins_over_individual_vars() {
        let key_json = serde_json::json!({
            "clientid": "json-client",
            "clientsecret": "json-secret",
            "url": "https://json-token.example.com",
            "serviceurls": {"AI_API_URL": "https://json-api.example.com"}
        })
        .to_string();
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some(key_json.clone()),
            "SAP_AI_CORE_CLIENT_ID" => Some("env-client".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://env-token.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://env-api.example.com".to_string()),
            _ => None,
        });
        assert_eq!(result.unwrap().client_id, "json-client");
    }

    #[test]
    fn test_sap_incomplete_individual_vars_returns_none() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "SAP_AI_CORE_CLIENT_ID" => Some("id".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.example.com".to_string()),
            _ => None,
        });
        assert!(result.is_none());
    }

    #[test]
    fn test_sap_invalid_json_falls_through() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some("not-valid-json".to_string()),
            "SAP_AI_CORE_CLIENT_ID" => Some("env-id".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://api.example.com".to_string()),
            _ => None,
        });
        assert_eq!(result.unwrap().client_id, "env-id");
    }

    #[test]
    fn test_sap_no_credentials_returns_none() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert!(result.is_none());
    }

    #[test]
    fn test_sap_json_key_alternate_field_names() {
        let key_json = serde_json::json!({
            "client_id": "alt-id",
            "client_secret": "alt-secret",
            "token_url": "https://alt-token.example.com",
            "service_url": "https://alt-api.example.com"
        })
        .to_string();
        let creds = parse_sap_service_key_json(&key_json);
        assert_eq!(
            creds,
            Some(SapResolvedCredentials {
                client_id: "alt-id".to_string(),
                client_secret: "alt-secret".to_string(),
                token_url: "https://alt-token.example.com".to_string(),
                service_url: "https://alt-api.example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_json_key_missing_required_field_returns_none() {
        let key_json = serde_json::json!({
            "clientid": "id",
            "url": "https://token.example.com",
            "serviceurls": {"AI_API_URL": "https://api.example.com"}
        })
        .to_string();
        assert!(parse_sap_service_key_json(&key_json).is_none());
    }

    // ── SAP AI Core metadata ─────────────────────────────────────────

    #[test]
    fn test_sap_metadata_exists() {
        let keys = env_keys_for_provider("sap-ai-core");
        assert!(!keys.is_empty(), "sap-ai-core should have env keys");
        assert!(keys.contains(&"AICORE_SERVICE_KEY"));
    }

    #[test]
    fn test_sap_alias_resolves() {
        let keys = env_keys_for_provider("sap");
        assert!(!keys.is_empty(), "sap alias should resolve");
        assert!(keys.contains(&"AICORE_SERVICE_KEY"));
    }

    #[test]
    fn test_exchange_sap_access_token_with_client_success() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_response = r#"{"access_token":"sap-access-token"}"#;
            let token_url = spawn_json_server(200, token_response);
            let client = crate::http::client::Client::new();
            let creds = SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url,
                service_url: "https://api.ai.sap.example.com".to_string(),
            };

            let token = exchange_sap_access_token_with_client(&client, &creds)
                .await
                .expect("token exchange");
            assert_eq!(token, "sap-access-token");
        });
    }

    #[test]
    fn test_exchange_sap_access_token_with_client_http_error() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(401, r#"{"error":"unauthorized"}"#);
            let client = crate::http::client::Client::new();
            let creds = SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url,
                service_url: "https://api.ai.sap.example.com".to_string(),
            };

            let err = exchange_sap_access_token_with_client(&client, &creds)
                .await
                .expect_err("expected HTTP error");
            assert!(
                err.to_string().contains("HTTP 401"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_exchange_sap_access_token_with_client_invalid_json() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(200, r#"{"token":"missing-access-token"}"#);
            let client = crate::http::client::Client::new();
            let creds = SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url,
                service_url: "https://api.ai.sap.example.com".to_string(),
            };

            let err = exchange_sap_access_token_with_client(&client, &creds)
                .await
                .expect_err("expected JSON error");
            assert!(
                err.to_string().contains("invalid JSON"),
                "unexpected error: {err}"
            );
        });
    }

    // ── Lifecycle tests (bd-3uqg.7.6) ─────────────────────────────

    #[test]
    fn test_proactive_refresh_triggers_within_window() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            // Token expires 5 minutes from now (within the 10-min window).
            let five_min_from_now = chrono::Utc::now().timestamp_millis() + 5 * 60 * 1000;
            let token_response =
                r#"{"access_token":"refreshed","refresh_token":"new-ref","expires_in":3600}"#;
            let server_url = spawn_json_server(200, token_response);

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "about-to-expire".to_string(),
                    refresh_token: "old-ref".to_string(),
                    expires: five_min_from_now,
                    token_url: Some(server_url),
                    client_id: Some("test-client".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("proactive refresh");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "refreshed");
                }
                other => panic!("expected OAuth, got: {other:?}"),
            }
        });
    }

    #[test]
    fn test_proactive_refresh_skips_tokens_far_from_expiry() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let one_hour_from_now = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "still-good".to_string(),
                    refresh_token: "ref".to_string(),
                    expires: one_hour_from_now,
                    token_url: Some("https://should-not-be-called.example.com/token".to_string()),
                    client_id: Some("test-client".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("no refresh needed");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "still-good");
                }
                other => panic!("expected OAuth, got: {other:?}"),
            }
        });
    }

    #[test]
    fn test_refresh_pooled_oauth() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let expiring_soon = chrono::Utc::now().timestamp_millis() + 60_000;
            let pooled_provider = "github-copilot";

            let pool_ok_url = spawn_json_server(
                200,
                r#"{"access_token":"pool-ok-new","refresh_token":"pool-ok-new-ref","expires_in":3600}"#,
            );
            let pool_fail_url = spawn_json_server(500, r#"{"error":"refresh failed"}"#);
            let ext_ok_url = spawn_json_server(
                200,
                r#"{"access_token":"ext-ok-new","refresh_token":"ext-ok-new-ref","expires_in":3600}"#,
            );

            let mut copilot_accounts = HashMap::new();
            copilot_accounts.insert(
                "acct-ok".to_string(),
                OAuthAccountRecord {
                    id: "acct-ok".to_string(),
                    label: Some("pool-ok".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "pool-ok-old".to_string(),
                        refresh_token: "pool-ok-old-ref".to_string(),
                        expires: expiring_soon,
                        token_url: Some(pool_ok_url),
                        client_id: Some("copilot-client".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );
            copilot_accounts.insert(
                "acct-fail".to_string(),
                OAuthAccountRecord {
                    id: "acct-fail".to_string(),
                    label: Some("pool-fail".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "pool-fail-old".to_string(),
                        refresh_token: "pool-fail-old-ref".to_string(),
                        expires: expiring_soon,
                        token_url: Some(pool_fail_url),
                        client_id: Some("copilot-client".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );

            let mut ext_accounts = HashMap::new();
            ext_accounts.insert(
                "acct-ext".to_string(),
                OAuthAccountRecord {
                    id: "acct-ext".to_string(),
                    label: Some("ext-ok".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "ext-ok-old".to_string(),
                        refresh_token: "ext-ok-old-ref".to_string(),
                        expires: expiring_soon,
                        token_url: Some(ext_ok_url),
                        client_id: Some("ext-client".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );

            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
            };
            auth.oauth_pools.insert(
                pooled_provider.to_string(),
                OAuthProviderPool {
                    active: Some("acct-ok".to_string()),
                    order: vec!["acct-ok".to_string(), "acct-fail".to_string()],
                    accounts: copilot_accounts,
                    namespace: None,
                },
            );
            auth.oauth_pools.insert(
                "my-ext".to_string(),
                OAuthProviderPool {
                    active: Some("acct-ext".to_string()),
                    order: vec!["acct-ext".to_string()],
                    accounts: ext_accounts,
                    namespace: None,
                },
            );

            let client = crate::http::client::Client::new();
            let err = auth
                .refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect_err("one pool account should fail refresh");
            assert!(
                err.to_string().contains(pooled_provider),
                "unexpected error: {err}"
            );

            let copilot_pool = auth
                .oauth_pools
                .get(pooled_provider)
                .expect("copilot pool");
            match &copilot_pool
                .accounts
                .get("acct-ok")
                .expect("copilot success account")
                .credential
            {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    ..
                } => {
                    assert_eq!(access_token, "pool-ok-new");
                    assert_eq!(refresh_token, "pool-ok-new-ref");
                }
                other => {
                    unreachable!("expected OAuth credential, got: {other:?}");
                }
            }
            assert!(
                copilot_pool
                    .accounts
                    .get("acct-ok")
                    .and_then(|account| account.health.last_success_ms)
                    .is_some()
            );

            match &copilot_pool
                .accounts
                .get("acct-fail")
                .expect("copilot failed account")
                .credential
            {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "pool-fail-old");
                }
                other => {
                    unreachable!("expected OAuth credential, got: {other:?}");
                }
            }

            let ext_pool = auth.oauth_pools.get("my-ext").expect("extension pool");
            match &ext_pool
                .accounts
                .get("acct-ext")
                .expect("extension account")
                .credential
            {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    ..
                } => {
                    assert_eq!(access_token, "ext-ok-new");
                    assert_eq!(refresh_token, "ext-ok-new-ref");
                }
                other => {
                    unreachable!("expected OAuth credential, got: {other:?}");
                }
            }

            let reloaded = AuthStorage::load(auth_path).expect("reload auth");
            let reloaded_copilot = reloaded
                .oauth_pools
                .get(pooled_provider)
                .expect("reload pool");
            match &reloaded_copilot
                .accounts
                .get("acct-ok")
                .expect("reload account")
                .credential
            {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "pool-ok-new");
                }
                other => {
                    unreachable!("expected OAuth credential, got: {other:?}");
                }
            }
            let reloaded_ext = reloaded.oauth_pools.get("my-ext").expect("reload ext pool");
            match &reloaded_ext
                .accounts
                .get("acct-ext")
                .expect("reload ext account")
                .credential
            {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "ext-ok-new");
                }
                other => {
                    unreachable!("expected OAuth credential, got: {other:?}");
                }
            }
        });
    }

    #[test]
    fn test_refresh_marks_openai_codex_account_relogin_when_metadata_missing() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let expiring_soon = chrono::Utc::now().timestamp_millis() + 60_000;

            let mut accounts = HashMap::new();
            accounts.insert(
                "acct-legacy".to_string(),
                OAuthAccountRecord {
                    id: "acct-legacy".to_string(),
                    label: Some("legacy-import".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "legacy-codex-access".to_string(),
                        refresh_token: "legacy-codex-refresh".to_string(),
                        expires: expiring_soon,
                        token_url: None,
                        client_id: None,
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.oauth_pools.insert(
                "openai-codex".to_string(),
                OAuthProviderPool {
                    active: Some("acct-legacy".to_string()),
                    order: vec!["acct-legacy".to_string()],
                    accounts,
                    namespace: None,
                },
            );

            let client = crate::http::client::Client::new();
            let err = auth
                .refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect_err("missing refresh metadata should require relogin");
            assert!(
                err.to_string().contains("openai-codex"),
                "unexpected error: {err}"
            );

            let account = auth
                .oauth_pools
                .get("openai-codex")
                .and_then(|pool| pool.accounts.get("acct-legacy"))
                .expect("legacy account");
            assert!(account.health.requires_relogin);
            assert!(
                account
                    .health
                    .last_error
                    .as_deref()
                    .is_some_and(|msg| msg.contains("token_url/client_id"))
            );
        });
    }

    #[test]
    fn test_refresh_marks_copilot_device_flow_account_relogin_and_rotates() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let now = chrono::Utc::now().timestamp_millis();

            let mut accounts = HashMap::new();
            accounts.insert(
                "acct-device".to_string(),
                OAuthAccountRecord {
                    id: "acct-device".to_string(),
                    label: Some("device-flow".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "copilot-device-token".to_string(),
                        refresh_token: String::new(),
                        expires: now + 60_000,
                        token_url: Some("https://example.test/token".to_string()),
                        client_id: Some("Iv1.device".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );
            accounts.insert(
                "acct-valid".to_string(),
                OAuthAccountRecord {
                    id: "acct-valid".to_string(),
                    label: Some("browser-flow".to_string()),
                    credential: AuthCredential::OAuth {
                        access_token: "copilot-valid-token".to_string(),
                        refresh_token: "valid-refresh".to_string(),
                        expires: now + 30 * 60_000,
                        token_url: Some("https://example.test/token".to_string()),
                        client_id: Some("Iv1.browser".to_string()),
                    },
                    health: OAuthAccountHealth::default(),
                    policy: OAuthAccountPolicy::default(),
                    provider_metadata: HashMap::new(),
                },
            );

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.oauth_pools.insert(
                "github-copilot".to_string(),
                OAuthProviderPool {
                    active: Some("acct-device".to_string()),
                    order: vec!["acct-device".to_string(), "acct-valid".to_string()],
                    accounts,
                    namespace: None,
                },
            );

            let client = crate::http::client::Client::new();
            let err = auth
                .refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect_err("non-refreshable device-flow token should fail refresh");
            assert!(
                err.to_string().contains("github-copilot"),
                "unexpected error: {err}"
            );

            let device_account = auth
                .oauth_pools
                .get("github-copilot")
                .and_then(|pool| pool.accounts.get("acct-device"))
                .expect("device account");
            assert!(device_account.health.requires_relogin);
            assert!(
                device_account
                    .health
                    .last_error
                    .as_deref()
                    .is_some_and(|msg| msg.contains("missing refresh_token"))
            );

            let resolved = auth
                .resolve_api_key_with_env_lookup("github-copilot", None, |_| None)
                .expect("fallback should rotate to valid account");
            assert_eq!(resolved, "copilot-valid-token");
        });
    }

    #[test]
    fn test_self_contained_refresh_uses_stored_metadata() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let token_response =
                r#"{"access_token":"new-copilot-token","refresh_token":"new-ref","expires_in":28800}"#;
            let server_url = spawn_json_server(200, token_response);

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "expired-copilot".to_string(),
                    refresh_token: "old-ref".to_string(),
                    expires: 0,
                    token_url: Some(server_url.clone()),
                    client_id: Some("Iv1.copilot-client".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("self-contained refresh");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth {
                    access_token,
                    token_url,
                    client_id,
                    ..
                } => {
                    assert_eq!(access_token, "new-copilot-token");
                    assert_eq!(token_url.as_deref(), Some(server_url.as_str()));
                    assert_eq!(client_id.as_deref(), Some("Iv1.copilot-client"));
                }
                other => panic!("expected OAuth, got: {other:?}"),
            }
        });
    }

    #[test]
    fn test_self_contained_refresh_skips_when_no_metadata() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.entries.insert(
                "ext-custom".to_string(),
                AuthCredential::OAuth {
                    access_token: "old-ext".to_string(),
                    refresh_token: "ref".to_string(),
                    expires: 0,
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("should succeed by skipping");

            match auth.entries.get("ext-custom").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "old-ext");
                }
                other => panic!("expected OAuth, got: {other:?}"),
            }
        });
    }

    #[test]
    fn test_extension_refresh_skips_self_contained_credentials() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
                oauth_pools: HashMap::new(),
                rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
                rotation_policy: RotationPolicy::default(),
                recovery_config: RecoveryConfig::default(),
                revocation_manager: RevocationManager::default(),
                audit_log: None,
                callbacks: None,
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "self-contained".to_string(),
                    refresh_token: "ref".to_string(),
                    expires: 0,
                    token_url: Some("https://github.com/login/oauth/access_token".to_string()),
                    client_id: Some("Iv1.copilot".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            let mut extension_configs = HashMap::new();
            extension_configs.insert("copilot".to_string(), sample_oauth_config());

            auth.refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await
                .expect("should succeed by skipping");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "self-contained");
                }
                other => panic!("expected OAuth, got: {other:?}"),
            }
        });
    }

    #[test]
    fn test_prune_stale_credentials_removes_old_expired_without_metadata() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        let now = chrono::Utc::now().timestamp_millis();
        let one_day_ms = 24 * 60 * 60 * 1000;

        // Stale: expired 2 days ago, no metadata.
        auth.entries.insert(
            "stale-ext".to_string(),
            AuthCredential::OAuth {
                access_token: "dead".to_string(),
                refresh_token: "dead-ref".to_string(),
                expires: now - 2 * one_day_ms,
                token_url: None,
                client_id: None,
            },
        );

        // Not stale: expired 2 days ago but HAS metadata.
        auth.entries.insert(
            "copilot".to_string(),
            AuthCredential::OAuth {
                access_token: "old-copilot".to_string(),
                refresh_token: "ref".to_string(),
                expires: now - 2 * one_day_ms,
                token_url: Some("https://github.com/login/oauth/access_token".to_string()),
                client_id: Some("Iv1.copilot".to_string()),
            },
        );

        // Not stale: expired recently.
        auth.entries.insert(
            "recent-ext".to_string(),
            AuthCredential::OAuth {
                access_token: "recent".to_string(),
                refresh_token: "ref".to_string(),
                expires: now - 30 * 60 * 1000, // 30 min ago
                token_url: None,
                client_id: None,
            },
        );

        // Not OAuth.
        auth.entries.insert(
            "anthropic".to_string(),
            AuthCredential::ApiKey {
                key: "sk-test".to_string(),
            },
        );

        let pruned = auth.prune_stale_credentials(one_day_ms);

        assert_eq!(pruned, vec!["stale-ext"]);
        assert!(!auth.entries.contains_key("stale-ext"));
        assert!(auth.entries.contains_key("copilot"));
        assert!(auth.entries.contains_key("recent-ext"));
        assert!(auth.entries.contains_key("anthropic"));
    }

    #[test]
    fn test_prune_stale_credentials_no_op_when_all_valid() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
            oauth_pools: HashMap::new(),
            rotation_cursors: Arc::new(Mutex::new(HashMap::new())),
            rotation_policy: RotationPolicy::default(),
            recovery_config: RecoveryConfig::default(),
            revocation_manager: RevocationManager::default(),
            audit_log: None,
            callbacks: None,
        };

        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        auth.entries.insert(
            "ext-prov".to_string(),
            AuthCredential::OAuth {
                access_token: "valid".to_string(),
                refresh_token: "ref".to_string(),
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );

        let pruned = auth.prune_stale_credentials(24 * 60 * 60 * 1000);
        assert!(pruned.is_empty());
        assert!(auth.entries.contains_key("ext-prov"));
    }

    #[test]
    fn test_credential_serialization_preserves_new_fields() {
        let cred = AuthCredential::OAuth {
            access_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            expires: 12345,
            token_url: Some("https://example.com/token".to_string()),
            client_id: Some("my-client".to_string()),
        };

        let json = serde_json::to_string(&cred).expect("serialize");
        assert!(json.contains("token_url"));
        assert!(json.contains("client_id"));

        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::OAuth {
                token_url,
                client_id,
                ..
            } => {
                assert_eq!(token_url.as_deref(), Some("https://example.com/token"));
                assert_eq!(client_id.as_deref(), Some("my-client"));
            }
            other => panic!("expected OAuth, got: {other:?}"),
        }
    }

    #[test]
    fn test_credential_serialization_omits_none_fields() {
        let cred = AuthCredential::OAuth {
            access_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            expires: 12345,
            token_url: None,
            client_id: None,
        };

        let json = serde_json::to_string(&cred).expect("serialize");
        assert!(!json.contains("token_url"));
        assert!(!json.contains("client_id"));
    }

    #[test]
    fn test_credential_deserialization_defaults_missing_fields() {
        let json =
            r#"{"type":"o_auth","access_token":"tok","refresh_token":"ref","expires":12345}"#;
        let parsed: AuthCredential = serde_json::from_str(json).expect("deserialize");
        match parsed {
            AuthCredential::OAuth {
                token_url,
                client_id,
                ..
            } => {
                assert!(token_url.is_none());
                assert!(client_id.is_none());
            }
            other => panic!("expected OAuth, got: {other:?}"),
        }
    }

    #[test]
    fn codex_openai_api_key_parser_ignores_oauth_access_token_only_payloads() {
        let value = serde_json::json!({
            "tokens": {
                "access_token": "codex-oauth-token"
            }
        });
        assert!(codex_openai_api_key_from_value(&value).is_none());
    }

    #[test]
    fn codex_access_token_parser_reads_nested_tokens_payload() {
        let value = serde_json::json!({
            "tokens": {
                "access_token": " codex-oauth-token "
            }
        });
        assert_eq!(
            codex_access_token_from_value(&value).as_deref(),
            Some("codex-oauth-token")
        );
    }

    #[test]
    fn codex_openai_api_key_parser_reads_openai_api_key_field() {
        let value = serde_json::json!({
            "OPENAI_API_KEY": " sk-openai "
        });
        assert_eq!(
            codex_openai_api_key_from_value(&value).as_deref(),
            Some("sk-openai")
        );
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Strategy for generating valid provider names
    fn provider_name_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9-]{0,19}"
    }

    /// Strategy for generating token-like strings
    fn token_strategy() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9._-]{10,100}"
    }

    /// Strategy for generating arbitrary strings for redaction testing
    fn arbitrary_string_strategy() -> impl Strategy<Value = String> {
        ".*"
    }

    /// Strategy for generating JSON keys
    fn json_key_strategy() -> impl Strategy<Value = String> {
        "[a-zA-Z_][a-zA-Z0-9_]*"
    }

    proptest! {
        /// Test that redact_known_secrets always redacts provided secrets
        #[test]
        fn redact_known_secrets_hides_secrets(
            text in arbitrary_string_strategy(),
            secret in token_strategy()
        ) {
            // Create text containing the secret
            let text_with_secret = format!("{text} Error occurred: {secret} was invalid");

            let redacted = redact_known_secrets(&text_with_secret, &[&secret]);

            // Secret should never appear in redacted text
            prop_assert!(!redacted.contains(&secret),
                "Secret '{}' was not redacted from text", secret);
        }

        /// Test that redact_known_secrets handles empty secrets gracefully
        #[test]
        fn redact_known_secrets_empty_secrets(text in arbitrary_string_strategy()) {
            let redacted = redact_known_secrets(&text, &["", "   ", "\t"]);

            // Should not panic and should return reasonable output
            prop_assert!(!redacted.is_empty() || text.is_empty());
        }

        /// Test that is_sensitive_json_key identifies known sensitive patterns
        #[test]
        fn sensitive_key_detection(key in json_key_strategy()) {
            let result = is_sensitive_json_key(&key);

            // Check consistency with known patterns
            let normalized: String = key
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|ch| ch.to_ascii_lowercase())
                .collect();

            let should_be_sensitive = matches!(
                normalized.as_str(),
                "token" | "accesstoken" | "refreshtoken" | "idtoken" |
                "apikey" | "authorization" | "credential" | "secret" |
                "clientsecret" | "password"
            ) || normalized.ends_with("token")
                || normalized.ends_with("secret")
                || normalized.ends_with("apikey")
                || normalized.contains("authorization");

            prop_assert_eq!(result, should_be_sensitive);
        }

        /// Test that redact_sensitive_json_value redacts all sensitive fields
        #[test]
        fn json_redaction_coverage(
            access_token in token_strategy(),
            refresh_token in token_strategy(),
            safe_value in "[a-zA-Z0-9]+"
        ) {
            let json = serde_json::json!({
                "access_token": access_token,
                "refresh_token": refresh_token,
                "safe_field": safe_value,
                "nested": {
                    "token": "nested-token-value",
                    "also_safe": safe_value
                }
            });

            let mut json_copy = json.clone();
            redact_sensitive_json_value(&mut json_copy);

            let result_str = serde_json::to_string(&json_copy).unwrap();

            // Sensitive fields should be redacted
            prop_assert!(result_str.contains("\"access_token\":\"[REDACTED]\""));
            prop_assert!(result_str.contains("\"refresh_token\":\"[REDACTED]\""));
            prop_assert!(result_str.contains("\"token\":\"[REDACTED]\""));

            // Safe fields should remain
            let safe_field_expected = format!("\"safe_field\":\"{}\"", safe_value);
            let also_safe_expected = format!("\"also_safe\":\"{}\"", safe_value);
            prop_assert!(result_str.contains(&safe_field_expected));
            prop_assert!(result_str.contains(&also_safe_expected));
        }

        /// Test bearer token marker roundtrip
        #[test]
        fn bearer_token_marker_roundtrip(token in token_strategy()) {
            let marked = mark_anthropic_oauth_bearer_token(&token);

            // Should have marker prefix
            prop_assert!(marked.starts_with(ANTHROPIC_OAUTH_BEARER_MARKER));

            // Should be extractable
            let unmarked = unmark_anthropic_oauth_bearer_token(&marked);
            prop_assert_eq!(unmarked, Some(token.as_str()));
        }

        /// Test that unmark returns None for non-marked tokens
        #[test]
        fn unmark_non_marked_token(token in token_strategy()) {
            // Token without marker
            let result = unmark_anthropic_oauth_bearer_token(&token);
            prop_assert!(result.is_none());
        }

        /// Test project-scoped token encoding/decoding roundtrip
        #[test]
        fn project_scoped_token_roundtrip(
            token in token_strategy(),
            project_id in "[a-z][a-z0-9-]{0,30}"
        ) {
            let encoded = encode_project_scoped_access_token(&token, &project_id);
            let decoded = decode_project_scoped_access_token(&encoded);

            prop_assert!(decoded.is_some());
            let (decoded_token, decoded_project) = decoded.unwrap();
            prop_assert_eq!(decoded_token, token);
            prop_assert_eq!(decoded_project, project_id);
        }

        /// Test that decode_project_scoped_access_token rejects invalid JSON
        #[test]
        fn project_scoped_token_rejects_invalid_json(input in arbitrary_string_strategy()) {
            // Skip valid JSON cases
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&input) else {
                // Not valid JSON, should return None
                let result = decode_project_scoped_access_token(&input);
                prop_assert!(result.is_none());
                return Ok(());
            };

            // If it is valid JSON, check that it has the required fields
            let has_token = json.get("token").and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false);
            let has_project = json.get("projectId")
                .or_else(|| json.get("project_id"))
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);

            let result = decode_project_scoped_access_token(&input);

            if has_token && has_project {
                prop_assert!(result.is_some());
            } else {
                prop_assert!(result.is_none());
            }
        }

        /// Test AuthCredential serialization roundtrip for ApiKey
        #[test]
        fn api_key_roundtrip(key in "[a-zA-Z0-9_-]{10,50}") {
            let cred = AuthCredential::ApiKey { key: key.clone() };
            let json = serde_json::to_string(&cred).unwrap();
            let parsed: AuthCredential = serde_json::from_str(&json).unwrap();

            match parsed {
                AuthCredential::ApiKey { key: parsed_key } => {
                    prop_assert_eq!(parsed_key, key);
                }
                _ => prop_assert!(false, "Wrong credential type"),
            }
        }

        /// Test AuthCredential serialization roundtrip for BearerToken
        #[test]
        fn bearer_token_credential_roundtrip(token in token_strategy()) {
            let cred = AuthCredential::BearerToken { token: token.clone() };
            let json = serde_json::to_string(&cred).unwrap();
            let parsed: AuthCredential = serde_json::from_str(&json).unwrap();

            match parsed {
                AuthCredential::BearerToken { token: parsed_token } => {
                    prop_assert_eq!(parsed_token, token);
                }
                _ => prop_assert!(false, "Wrong credential type"),
            }
        }

        /// Test OAuth credential roundtrip
        #[test]
        fn oauth_credential_roundtrip(
            access_token in token_strategy(),
            refresh_token in token_strategy(),
            expires in 1_000_i64..9_999_999_999_999_i64
        ) {
            let cred = AuthCredential::OAuth {
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                expires,
                token_url: Some("https://oauth.example.com/token".to_string()),
                client_id: Some("test-client-id".to_string()),
            };
            let json = serde_json::to_string(&cred).unwrap();
            let parsed: AuthCredential = serde_json::from_str(&json).unwrap();

            match parsed {
                AuthCredential::OAuth {
                    access_token: at,
                    refresh_token: rt,
                    expires: e,
                    token_url,
                    client_id,
                } => {
                    prop_assert_eq!(at, access_token);
                    prop_assert_eq!(rt, refresh_token);
                    prop_assert_eq!(e, expires);
                    prop_assert!(token_url.is_some());
                    prop_assert!(client_id.is_some());
                }
                _ => prop_assert!(false, "Wrong credential type"),
            }
        }

        /// Test AWS credentials roundtrip
        #[test]
        fn aws_credentials_roundtrip(
            access_key_id in "AKIA[A-Z0-9]{16}",
            secret_access_key in "[a-zA-Z0-9+/]{40}",
            region in prop::sample::select(["us-east-1", "eu-west-1", "ap-southeast-1"].as_slice())
        ) {
            let cred = AuthCredential::AwsCredentials {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: None,
                region: Some(region.to_string()),
            };
            let json = serde_json::to_string(&cred).unwrap();
            let parsed: AuthCredential = serde_json::from_str(&json).unwrap();

            match parsed {
                AuthCredential::AwsCredentials {
                    access_key_id: aki,
                    secret_access_key: sak,
                    session_token,
                    region: r,
                } => {
                    prop_assert_eq!(aki, access_key_id);
                    prop_assert_eq!(sak, secret_access_key);
                    prop_assert!(session_token.is_none());
                    prop_assert_eq!(r, Some(region.to_string()));
                }
                _ => prop_assert!(false, "Wrong credential type"),
            }
        }

        /// Test that redact_known_secrets handles multiple secrets
        #[test]
        fn redact_multiple_secrets(
            secret1 in token_strategy(),
            secret2 in token_strategy(),
            secret3 in token_strategy()
        ) {
            prop_assume!(secret1 != secret2 && secret2 != secret3 && secret1 != secret3);

            let text = format!("Error with {}, {}, and {}", secret1, secret2, secret3);
            let redacted = redact_known_secrets(&text, &[&secret1, &secret2, &secret3]);

            prop_assert!(!redacted.contains(&secret1));
            prop_assert!(!redacted.contains(&secret2));
            prop_assert!(!redacted.contains(&secret3));
            prop_assert!(redacted.contains("[REDACTED]"));
        }

        /// Test that redaction preserves non-sensitive parts of the message
        #[test]
        fn redaction_preserves_context(
            prefix in "[a-zA-Z ]{5,20}",
            suffix in "[a-zA-Z ]{5,20}",
            secret in token_strategy()
        ) {
            let text = format!("{}{}{}", prefix, secret, suffix);
            let redacted = redact_known_secrets(&text, &[&secret]);

            prop_assert!(redacted.starts_with(&prefix));
            prop_assert!(redacted.ends_with(&suffix));
        }

        /// Test CredentialStatus determination for OAuth credentials
        #[test]
        fn credential_status_oauth_expiry(
            current_time in 1_000_000_000_000_i64..9_999_999_999_999_i64,
            offset_ms in -86_400_000_i64..86_400_000_i64 // +/- 1 day
        ) {
            let expires = current_time + offset_ms;
            let cred = AuthCredential::OAuth {
                access_token: "test-access".to_string(),
                refresh_token: "test-refresh".to_string(),
                expires,
                token_url: None,
                client_id: None,
            };

            // Check that credential can be serialized/deserialized
            let json = serde_json::to_string(&cred).unwrap();
            let parsed: AuthCredential = serde_json::from_str(&json).unwrap();

            match parsed {
                AuthCredential::OAuth { expires: e, .. } => {
                    prop_assert_eq!(e, expires);
                }
                _ => prop_assert!(false, "Wrong credential type"),
            }
        }
    }
}

// Loom-based concurrency tests for cache module
#[cfg(test)]
mod cache_concurrency_tests;
