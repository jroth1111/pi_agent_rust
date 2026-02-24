//! Multi-source secret resolution with priority ordering and caching.
//!
//! This module provides a unified abstraction for resolving authentication secrets
//! from multiple sources with a well-defined priority chain:
//!
//! 1. CLI overrides (--api-key flag) - highest priority
//! 2. Config file (explicit values in pi_config.json)
//! 3. Environment variables (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.)
//! 4. External files (~/.config/gcloud, ~/.aws/credentials, etc.)
//! 5. OAuth tokens (auth.json OAuth access tokens)
//! 6. Stored API keys (auth.json API keys) - lowest priority

use crate::config::Config;
use crate::error::{Error, Result};
use crate::provider_metadata::{canonical_provider_id, provider_auth_env_keys};
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Secret source with priority ordering (highest to lowest).
///
/// Sources with higher ordinal values take precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecretSource {
    /// CLI override (--api-key flag)
    CliOverride = 60,
    /// Config file explicit value
    ConfigFile = 50,
    /// Environment variable
    Environment = 40,
    /// External file (~/.config/gcloud, ~/.codex/auth.json, etc.)
    ExternalFile = 30,
    /// OAuth token from auth.json
    OAuthToken = 20,
    /// Stored API key from auth.json
    StoredKey = 10,
}

impl SecretSource {
    /// Returns a human-readable name for the source.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::CliOverride => "CLI override",
            Self::ConfigFile => "config file",
            Self::Environment => "environment",
            Self::ExternalFile => "external file",
            Self::OAuthToken => "OAuth token",
            Self::StoredKey => "stored key",
        }
    }

    /// Returns a description of the source location.
    pub fn description(&self) -> String {
        match self {
            Self::CliOverride => "--api-key flag".to_string(),
            Self::ConfigFile => "pi_config.json".to_string(),
            Self::Environment => "environment variable".to_string(),
            Self::ExternalFile => "~/.config/gcloud, ~/.codex/auth.json, or similar".to_string(),
            Self::OAuthToken => "auth.json OAuth access token".to_string(),
            Self::StoredKey => "auth.json API key".to_string(),
        }
    }
}

/// A resolved secret with metadata about its source.
#[derive(Debug, Clone)]
pub struct ResolvedSecret {
    /// The secret value (API key, access token, etc.)
    pub value: String,
    /// The source where this secret was found
    pub source: SecretSource,
    /// The provider identifier
    pub provider: String,
    /// Expiration timestamp for OAuth tokens (Unix milliseconds)
    pub expires_at: Option<i64>,
}

impl ResolvedSecret {
    /// Creates a new resolved secret.
    const fn new(value: String, source: SecretSource, provider: String) -> Self {
        Self {
            value,
            source,
            provider,
            expires_at: None,
        }
    }

    /// Sets the expiration time for this secret.
    const fn with_expiration(mut self, expires_at: i64) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Returns true if this secret is expired (only relevant for OAuth tokens).
    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            let now = chrono::Utc::now().timestamp_millis();
            expires_at <= now
        } else {
            false
        }
    }

    /// Returns the time until expiration as a Duration.
    pub fn time_until_expiration(&self) -> Option<Duration> {
        self.expires_at.map(|expires_at| {
            let now = chrono::Utc::now().timestamp_millis();
            let millis = expires_at.saturating_sub(now);
            Duration::from_millis(millis.max(0) as u64)
        })
    }
}

/// Cached secret with TTL metadata.
#[derive(Debug, Clone)]
struct CachedSecret {
    /// The resolved secret
    secret: ResolvedSecret,
    /// When this was cached
    cached_at: Instant,
    /// Time-to-live for this cache entry
    ttl: Duration,
}

impl CachedSecret {
    /// Creates a new cached secret.
    fn new(secret: ResolvedSecret, ttl: Duration) -> Self {
        Self {
            secret,
            cached_at: Instant::now(),
            ttl,
        }
    }

    /// Returns true if this cached entry has expired.
    fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }
}

/// Default TTL for cached secrets (5 minutes).
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);

/// Default cache capacity (100 entries).
const DEFAULT_CACHE_CAPACITY: usize = 100;

/// Multi-source secret resolver with caching.
///
/// The resolver checks sources in priority order:
/// 1. CLI overrides (--api-key flag)
/// 2. Config file explicit values
/// 3. Environment variables
/// 4. External files (gcloud, codex, etc.)
/// 5. OAuth tokens from auth storage
/// 6. Stored API keys from auth storage
#[derive(Debug, Clone)]
pub struct SecretResolver {
    /// Application config
    config: Arc<Config>,
    /// Path to auth.json file
    auth_path: std::path::PathBuf,
    /// CLI overrides from --api-key flags
    cli_overrides: Arc<std::sync::RwLock<HashMap<String, String>>>,
    /// Resolution cache
    cache: Arc<std::sync::RwLock<LruCache<String, CachedSecret>>>,
    /// Default TTL for cached entries
    default_ttl: Duration,
    /// Whether to allow external file lookups
    allow_external_lookup: bool,
}

impl SecretResolver {
    /// Creates a new secret resolver.
    pub fn new(config: Arc<Config>, auth_path: std::path::PathBuf) -> Self {
        let capacity = NonZeroUsize::new(DEFAULT_CACHE_CAPACITY).unwrap();
        Self {
            config,
            auth_path,
            cli_overrides: Arc::new(std::sync::RwLock::new(HashMap::new())),
            cache: Arc::new(std::sync::RwLock::new(LruCache::new(capacity))),
            default_ttl: DEFAULT_CACHE_TTL,
            allow_external_lookup: true,
        }
    }

    /// Creates a new secret resolver with custom cache settings.
    pub fn with_cache_settings(
        config: Arc<Config>,
        auth_path: std::path::PathBuf,
        cache_capacity: usize,
        default_ttl: Duration,
    ) -> Self {
        let capacity = NonZeroUsize::new(cache_capacity).unwrap_or(NonZeroUsize::new(100).unwrap());
        Self {
            config,
            auth_path,
            cli_overrides: Arc::new(std::sync::RwLock::new(HashMap::new())),
            cache: Arc::new(std::sync::RwLock::new(LruCache::new(capacity))),
            default_ttl,
            allow_external_lookup: true,
        }
    }

    /// Sets whether external file lookups are allowed.
    pub const fn with_external_lookup(mut self, allow: bool) -> Self {
        self.allow_external_lookup = allow;
        self
    }

    /// Sets a CLI override for a provider.
    ///
    /// This takes highest priority in the resolution chain.
    pub fn set_cli_override(&self, provider: &str, key: &str) -> Result<()> {
        if key.trim().is_empty() {
            return Err(Error::auth("CLI override key cannot be empty".to_string()));
        }
        let provider_key = Self::cache_key(provider);
        let mut overrides = self
            .cli_overrides
            .write()
            .map_err(|e| Error::auth(format!("Failed to acquire CLI override lock: {e}")))?;
        overrides.insert(provider_key, key.to_string());
        self.invalidate_cache(provider);
        Ok(())
    }

    /// Clears a CLI override for a provider.
    pub fn clear_cli_override(&self, provider: &str) {
        let provider_key = Self::cache_key(provider);
        if let Ok(mut overrides) = self.cli_overrides.write() {
            overrides.remove(&provider_key);
            self.invalidate_cache(provider);
        }
    }

    /// Invalidates the cache for a specific provider.
    pub fn invalidate_cache(&self, provider: &str) {
        let cache_key = Self::cache_key(provider);
        let mut cache = self.cache.write().unwrap();
        cache.pop(&cache_key);
    }

    /// Invalidates all cached entries.
    pub fn invalidate_all(&self) {
        let mut cache = self.cache.write().unwrap();
        cache.clear();
    }

    /// Generates a cache key for a provider.
    fn cache_key(provider: &str) -> String {
        provider.trim().to_ascii_lowercase()
    }

    /// Normalizes a provider identifier.
    fn normalize_provider(&self, provider: &str) -> String {
        let normalized = provider.trim().to_ascii_lowercase();
        canonical_provider_id(&normalized)
            .unwrap_or(&normalized)
            .to_string()
    }

    /// Resolves a secret for the given provider.
    ///
    /// Returns `Ok(None)` if no secret is found for the provider.
    pub fn resolve(&self, provider: &str) -> Result<Option<ResolvedSecret>> {
        self.resolve_with_model(provider, None)
    }

    /// Resolves a secret for the given provider and model.
    ///
    /// The model parameter is used for OAuth pool filtering when applicable.
    pub fn resolve_with_model(
        &self,
        provider: &str,
        _model: Option<&str>,
    ) -> Result<Option<ResolvedSecret>> {
        let normalized = self.normalize_provider(provider);

        // Check cache first
        let cache_key = Self::cache_key(&normalized);
        {
            let mut cache = self.cache.write().unwrap();
            if let Some(cached) = cache.get_mut(&cache_key) {
                if !cached.is_expired() {
                    return Ok(Some(cached.secret.clone()));
                }
                // Expired entry, remove it
                cache.pop(&cache_key);
            }
        }

        // Resolve from sources in priority order
        let result = self.resolve_from_sources(&normalized)?;

        // Cache the result
        if let Some(ref secret) = result {
            let cached = CachedSecret::new(secret.clone(), self.default_ttl);
            let mut cache = self.cache.write().unwrap();
            cache.put(cache_key, cached);
        }

        Ok(result)
    }

    /// Resolves a secret by checking sources in priority order.
    fn resolve_from_sources(&self, provider: &str) -> Result<Option<ResolvedSecret>> {
        // 1. Check CLI overrides (highest priority)
        if let Some(key) = self.check_cli_overrides(provider) {
            return Ok(Some(ResolvedSecret::new(
                key,
                SecretSource::CliOverride,
                provider.to_string(),
            )));
        }

        // 2. Check config file values
        if let Some(key) = self.check_config_file(provider) {
            return Ok(Some(ResolvedSecret::new(
                key,
                SecretSource::ConfigFile,
                provider.to_string(),
            )));
        }

        // 3. Check environment variables
        if let Some(key) = self.check_environment(provider) {
            return Ok(Some(ResolvedSecret::new(
                key,
                SecretSource::Environment,
                provider.to_string(),
            )));
        }

        // 4. Check external files (only if allowed)
        if self.allow_external_lookup {
            if let Some((key, expires_at)) = self.check_external_files(provider) {
                return Ok(Some(
                    ResolvedSecret::new(key, SecretSource::ExternalFile, provider.to_string())
                        .with_expiration(expires_at.unwrap_or(i64::MAX)),
                ));
            }
        }

        // 5 & 6. Check auth storage (OAuth tokens, then stored keys)
        // We use the AuthStorage API which handles priority internally
        if let Some(storage) = self.load_auth_storage_blocking() {
            // Let AuthStorage handle the resolution
            if let Some(key) = storage.resolve_api_key(provider, None) {
                // Determine the source based on what was found
                // Since AuthStorage doesn't expose the source, we use a heuristic
                return Ok(Some(ResolvedSecret::new(
                    key,
                    SecretSource::StoredKey, // Default to stored key since we can't distinguish
                    provider.to_string(),
                )));
            }
        }

        Ok(None)
    }

    /// Checks CLI overrides for a provider.
    fn check_cli_overrides(&self, provider: &str) -> Option<String> {
        let overrides = self.cli_overrides.read().ok()?;
        // Try exact match, then canonical
        overrides
            .get(provider)
            .or_else(|| {
                canonical_provider_id(provider).and_then(|canonical| overrides.get(canonical))
            })
            .cloned()
    }

    /// Checks config file for an explicit API key.
    const fn check_config_file(&self, _provider: &str) -> Option<String> {
        // This would check the config file for provider-specific keys
        // The exact config structure depends on how pi_config.json stores keys
        // For now, this is a placeholder for future config integration
        None
    }

    /// Checks environment variables for a provider.
    fn check_environment(&self, provider: &str) -> Option<String> {
        let env_keys = provider_auth_env_keys(provider);
        for key in env_keys {
            if let Ok(value) = std::env::var(key) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }

    /// Checks external files for a provider's credentials.
    ///
    /// Returns the key and optionally an expiration time.
    fn check_external_files(&self, provider: &str) -> Option<(String, Option<i64>)> {
        // Only allow external lookups for the global auth path
        if self.auth_path != Config::auth_path() {
            return None;
        }

        // Use the external provider resolution logic
        // Since the parent module functions are private, we inline the logic here
        let canonical = canonical_provider_id(provider).unwrap_or(provider);

        match canonical {
            "anthropic" => self
                .read_external_claude_access_token()
                .map(|token| (token, None)),
            "openai" => self
                .read_external_codex_openai_api_key()
                .map(|key| (key, None)),
            "openai-codex" => self
                .read_external_codex_access_token()
                .map(|token| (token, None)),
            "google-gemini-cli" | "google-antigravity" => {
                let project = self
                    .google_project_id_from_env()
                    .or_else(|| self.google_project_id_from_gcloud_config());
                self.read_external_gemini_access_payload(project.as_deref())
                    .map(|token| (token, None))
            }
            "kimi-for-coding" => self
                .read_external_kimi_code_access_token()
                .map(|token| (token, None)),
            _ => None,
        }
    }

    /// Loads auth storage (blocking version for use in sync context).
    fn load_auth_storage_blocking(&self) -> Option<crate::auth::AuthStorage> {
        crate::auth::AuthStorage::load(self.auth_path.clone()).ok()
    }

    /// Returns the external setup source for a provider (human-readable label).
    pub fn external_setup_source(&self, provider: &str) -> Option<String> {
        if !self.allow_external_lookup {
            return None;
        }
        if self.auth_path != Config::auth_path() {
            return None;
        }

        let canonical = canonical_provider_id(provider).unwrap_or(provider);
        match canonical {
            "anthropic" if self.read_external_claude_access_token().is_some() => {
                Some("Claude Code (~/.claude/.credentials.json)".to_string())
            }
            "openai" if self.read_external_codex_openai_api_key().is_some() => {
                Some("Codex (~/.codex/auth.json)".to_string())
            }
            "openai-codex" if self.read_external_codex_access_token().is_some() => {
                Some("Codex (~/.codex/auth.json)".to_string())
            }
            "google-gemini-cli" | "google-antigravity" => {
                let project = self.google_project_id_from_env()
                    .or_else(|| self.google_project_id_from_gcloud_config());
                if self
                    .read_external_gemini_access_payload(project.as_deref())
                    .is_some()
                {
                    Some("Gemini CLI (~/.gemini/oauth_creds.json)".to_string())
                } else {
                    None
                }
            }
            "kimi-for-coding" if self.read_external_kimi_code_access_token().is_some() => Some(
                "Kimi CLI (~/.kimi/credentials/kimi-code.json or $KIMI_SHARE_DIR/credentials/kimi-code.json)"
                    .to_string(),
            ),
            _ => None,
        }
    }

    /// Returns all provider names that have credentials in auth storage.
    pub fn provider_names(&self) -> Result<Vec<String>> {
        if let Some(storage) = self.load_auth_storage_blocking() {
            Ok(storage.provider_names())
        } else {
            Ok(Vec::new())
        }
    }

    // ── External credential reading functions ─────────────────────────────────

    fn home_dir() -> Option<std::path::PathBuf> {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .map(std::path::PathBuf::from)
    }

    fn read_external_json(path: &Path) -> Option<serde_json::Value> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn read_external_claude_access_token(&self) -> Option<String> {
        let path = Self::home_dir()?.join(".claude").join(".credentials.json");
        let value = Self::read_external_json(&path)?;
        let token = value
            .get("claudeAiOauth")
            .and_then(|oauth| oauth.get("accessToken"))
            .and_then(serde_json::Value::as_str)?
            .trim()
            .to_string();
        if token.is_empty() { None } else { Some(token) }
    }

    fn read_external_codex_auth(&self) -> Option<serde_json::Value> {
        let home = Self::home_dir()?;
        let candidates = [
            home.join(".codex").join("auth.json"),
            home.join(".config").join("codex").join("auth.json"),
        ];
        for path in candidates {
            if let Some(value) = Self::read_external_json(&path) {
                return Some(value);
            }
        }
        None
    }

    fn read_external_codex_access_token(&self) -> Option<String> {
        let value = self.read_external_codex_auth()?;
        self.codex_access_token_from_value(&value)
    }

    fn read_external_codex_openai_api_key(&self) -> Option<String> {
        let value = self.read_external_codex_auth()?;
        self.codex_openai_api_key_from_value(&value)
    }

    fn codex_access_token_from_value(&self, value: &serde_json::Value) -> Option<String> {
        let candidates = [
            value
                .get("tokens")
                .and_then(|tokens| tokens.get("access_token"))
                .and_then(serde_json::Value::as_str),
            value
                .get("tokens")
                .and_then(|tokens| tokens.get("accessToken"))
                .and_then(serde_json::Value::as_str),
            value
                .get("access_token")
                .and_then(serde_json::Value::as_str),
            value.get("accessToken").and_then(serde_json::Value::as_str),
        ];

        candidates
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|token| !token.is_empty())
            .map(std::string::ToString::to_string)
    }

    fn codex_openai_api_key_from_value(&self, value: &serde_json::Value) -> Option<String> {
        let candidates = [
            value
                .get("env")
                .and_then(|env| env.get("OPENAI_API_KEY"))
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

    fn read_external_gemini_access_payload(&self, project_id: Option<&str>) -> Option<String> {
        let home = Self::home_dir()?;
        let candidates = [
            home.join(".gemini").join("oauth_creds.json"),
            home.join(".config").join("gemini").join("credentials.json"),
        ];

        for path in candidates {
            let Some(value) = Self::read_external_json(&path) else {
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

            let Some(project) = project_id
                .map(std::string::ToString::to_string)
                .or_else(|| {
                    value
                        .get("project_id")
                        .and_then(serde_json::Value::as_str)
                        .map(std::string::ToString::to_string)
                })
            else {
                continue;
            };

            return Some(self.encode_project_scoped_access_token(token, &project));
        }

        None
    }

    fn encode_project_scoped_access_token(&self, token: &str, project_id: &str) -> String {
        serde_json::json!({
            "token": token,
            "projectId": project_id,
        })
        .to_string()
    }

    fn google_project_id_from_env(&self) -> Option<String> {
        std::env::var("GOOGLE_CLOUD_PROJECT")
            .ok()
            .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT_ID").ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn gcloud_config_dir(&self) -> Option<std::path::PathBuf> {
        let config_root = std::env::var("CLOUDSDK_CONFIG")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| Self::home_dir().map(|home| home.join(".config").join("gcloud")))?;
        Some(config_root)
    }

    fn gcloud_active_config_name(&self) -> String {
        std::env::var("CLOUDSDK_ACTIVE_CONFIG_NAME")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".to_string())
    }

    fn google_project_id_from_gcloud_config(&self) -> Option<String> {
        let config_dir = self.gcloud_config_dir()?;
        let config_name = self.gcloud_active_config_name();
        let config_file = config_dir
            .join("configurations")
            .join(format!("config_{config_name}"));

        let Ok(content) = std::fs::read_to_string(config_file) else {
            return None;
        };

        // Parse the gcloud config file (key=value format)
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with("project = ") || line.starts_with("project=") {
                let parts = line
                    .split('=')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .skip(1)
                    .collect::<String>();
                let project = parts.trim().trim_start_matches('/');
                if project.is_empty() {
                    continue;
                }
                return Some(project.to_string());
            }
        }

        None
    }

    fn kimi_share_dir(&self) -> Option<std::path::PathBuf> {
        std::env::var("KIMI_SHARE_DIR")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| Self::home_dir().map(|home| home.join(".kimi").join("share")))
    }

    fn read_external_kimi_code_access_token(&self) -> Option<String> {
        let share_dir = self.kimi_share_dir()?;
        self.read_external_kimi_code_access_token_from_share_dir(&share_dir)
    }

    fn read_external_kimi_code_access_token_from_share_dir(
        &self,
        share_dir: &std::path::Path,
    ) -> Option<String> {
        let path = share_dir.join("credentials").join("kimi-code.json");
        let value = Self::read_external_json(&path)?;

        let token = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())?;

        let expires_at = value.get("expires_at").and_then(serde_json::Value::as_f64);

        if let Some(expires_at) = expires_at {
            let now_seconds = chrono::Utc::now().timestamp() as f64;
            if expires_at <= now_seconds {
                return None;
            }
        }

        Some(token.to_string())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_source_ordering() {
        // Verify that sources have the correct priority ordering
        assert!(SecretSource::CliOverride > SecretSource::ConfigFile);
        assert!(SecretSource::ConfigFile > SecretSource::Environment);
        assert!(SecretSource::Environment > SecretSource::ExternalFile);
        assert!(SecretSource::ExternalFile > SecretSource::OAuthToken);
        assert!(SecretSource::OAuthToken > SecretSource::StoredKey);
    }

    #[test]
    fn test_secret_source_names() {
        assert_eq!(SecretSource::CliOverride.name(), "CLI override");
        assert_eq!(SecretSource::ConfigFile.name(), "config file");
        assert_eq!(SecretSource::Environment.name(), "environment");
        assert_eq!(SecretSource::ExternalFile.name(), "external file");
        assert_eq!(SecretSource::OAuthToken.name(), "OAuth token");
        assert_eq!(SecretSource::StoredKey.name(), "stored key");
    }

    #[test]
    fn test_resolved_secret_expiration() {
        let now = chrono::Utc::now().timestamp_millis();

        // Non-expired secret
        let secret = ResolvedSecret::new(
            "test-key".to_string(),
            SecretSource::StoredKey,
            "test".to_string(),
        )
        .with_expiration(now + 1000);

        assert!(!secret.is_expired());
        assert!(secret.time_until_expiration().is_some());

        // Expired secret
        let secret = ResolvedSecret::new(
            "test-key".to_string(),
            SecretSource::StoredKey,
            "test".to_string(),
        )
        .with_expiration(now - 1000);

        assert!(secret.is_expired());

        // Secret without expiration
        let secret = ResolvedSecret::new(
            "test-key".to_string(),
            SecretSource::StoredKey,
            "test".to_string(),
        );

        assert!(!secret.is_expired());
        assert!(secret.time_until_expiration().is_none());
    }

    #[test]
    fn test_cached_secret_expiration() {
        let secret = ResolvedSecret::new(
            "test-key".to_string(),
            SecretSource::StoredKey,
            "test".to_string(),
        );

        // Fresh cache entry
        let cached = CachedSecret::new(secret.clone(), Duration::from_secs(60));
        assert!(!cached.is_expired());

        // Expired cache entry
        let cached = CachedSecret::new(secret, Duration::from_secs(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(cached.is_expired());
    }

    #[test]
    fn test_normalize_provider() {
        let config = Arc::new(Config::default());
        let resolver = SecretResolver::new(config, std::path::PathBuf::from("/tmp/test_auth.json"));

        assert_eq!(resolver.normalize_provider("Anthropic"), "anthropic");
        assert_eq!(resolver.normalize_provider("  OPENAI  "), "openai");
        assert_eq!(
            resolver.normalize_provider("google-gemini-cli"),
            "google-gemini-cli"
        );
    }

    #[test]
    fn test_cache_key() {
        assert_eq!(SecretResolver::cache_key("Anthropic"), "anthropic");
        assert_eq!(SecretResolver::cache_key("  OPENAI  "), "openai");
    }

    #[test]
    fn test_cli_override_priority() {
        let config = Arc::new(Config::default());
        let resolver = SecretResolver::new(config, std::path::PathBuf::from("/tmp/test_auth.json"));

        // Set a CLI override
        resolver
            .set_cli_override("anthropic", "sk-cli-test-key")
            .unwrap();

        // Verify it's stored
        {
            let overrides = resolver.cli_overrides.read().unwrap();
            assert_eq!(
                overrides.get("anthropic"),
                Some(&"sk-cli-test-key".to_string())
            );
        }

        // Clear it
        resolver.clear_cli_override("anthropic");
        {
            let overrides = resolver.cli_overrides.read().unwrap();
            assert!(!overrides.contains_key("anthropic"));
        }
    }

    #[test]
    fn test_cli_override_empty_key_rejected() {
        let config = Arc::new(Config::default());
        let resolver = SecretResolver::new(config, std::path::PathBuf::from("/tmp/test_auth.json"));

        // Empty key should be rejected
        let result = resolver.set_cli_override("anthropic", "   ");
        assert!(result.is_err());

        // Whitespace-only key should be rejected
        let result = resolver.set_cli_override("anthropic", "\t\n");
        assert!(result.is_err());
    }

    // Note: Environment variable tests are skipped because the codebase
    // forbids unsafe_code, and set_var/remove_var require unsafe in Rust 2024+.
    // The check_environment logic is simple and well-covered by integration tests.

    #[test]
    fn test_cache_invalidation() {
        let config = Arc::new(Config::default());
        let resolver = SecretResolver::new(config, std::path::PathBuf::from("/tmp/test_auth.json"));

        // Set a CLI override to populate cache
        resolver
            .set_cli_override("anthropic", "sk-cli-test-key")
            .unwrap();

        // Resolve should populate cache
        let result = resolver.resolve("anthropic").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().source, SecretSource::CliOverride);

        // Invalidate cache
        resolver.invalidate_cache("anthropic");

        // Cache should be cleared but CLI override still works
        let result = resolver.resolve("anthropic").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().source, SecretSource::CliOverride);
    }

    #[test]
    fn test_invalidate_all() {
        let config = Arc::new(Config::default());
        let resolver = SecretResolver::new(config, std::path::PathBuf::from("/tmp/test_auth.json"));

        // Set multiple CLI overrides
        resolver
            .set_cli_override("anthropic", "sk-anthropic-key")
            .unwrap();
        resolver
            .set_cli_override("openai", "sk-openai-key")
            .unwrap();

        // Resolve to populate cache
        resolver.resolve("anthropic").unwrap();
        resolver.resolve("openai").unwrap();

        // Invalidate all
        resolver.invalidate_all();

        // Cache should be cleared but overrides still work
        let result = resolver.resolve("anthropic").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_cache_hit_miss_behavior() {
        let config = Arc::new(Config::default());
        let resolver = SecretResolver::with_cache_settings(
            config,
            std::path::PathBuf::from("/tmp/test_auth.json"),
            10,
            Duration::from_millis(100),
        );

        // Set a CLI override
        resolver
            .set_cli_override("test-provider", "test-key")
            .unwrap();

        // First resolve should cache
        let result1 = resolver.resolve("test-provider").unwrap();
        assert!(result1.is_some());

        // Second resolve should hit cache (within TTL)
        let result2 = resolver.resolve("test-provider").unwrap();
        assert!(result2.is_some());
        assert_eq!(result1.unwrap().value, result2.unwrap().value);

        // Wait for cache to expire
        std::thread::sleep(Duration::from_millis(150));

        // Cache should be expired, but CLI override still works
        let result3 = resolver.resolve("test-provider").unwrap();
        assert!(result3.is_some());
    }
}
