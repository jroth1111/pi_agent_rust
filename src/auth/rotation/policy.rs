//! Rotation policy configuration and strategies.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Strategy for rotating OAuth tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum RotationStrategy {
    /// Rotate only when token expires (default).
    #[default]
    OnExpiry,

    /// Rotate proactively before expiry.
    Proactive,

    /// Rotate on a fixed time interval.
    FixedInterval,

    /// Rotate based on usage entropy patterns.
    EntropyBased,

    /// Combination of proactive, entropy, and usage.
    Hybrid,
}

/// Reason for a token rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationReason {
    /// Token expired.
    Expired,

    /// Proactive refresh before expiry.
    Proactive,

    /// Time-based interval rotation.
    Interval,

    /// Entropy-based rotation triggered.
    EntropyThreshold,

    /// Maximum usage count exceeded.
    MaxUsage,

    /// HTTP error triggered rotation.
    ErrorTriggered,

    /// Manual rotation request.
    Manual,

    /// Security-triggered rotation.
    Security,

    /// Pool rebalancing.
    Rebalance,
}

/// Provider-specific rotation configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRotationConfig {
    /// Rotation strategy for this provider.
    #[serde(default)]
    pub strategy: RotationStrategy,

    /// Time interval for fixed-interval rotation (milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<i64>,

    /// HTTP status codes that should trigger rotation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rotate_on_errors: Vec<u16>,

    /// Maximum usage count before rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usage_count: Option<u64>,

    /// Entropy threshold for this provider (0.0-1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entropy_threshold: Option<f64>,

    /// Proactive refresh window (milliseconds before expiry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive_window_ms: Option<i64>,

    /// Whether this provider is enabled for rotation.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for ProviderRotationConfig {
    fn default() -> Self {
        Self {
            strategy: RotationStrategy::default(),
            interval_ms: None,
            rotate_on_errors: vec![401, 403],
            max_usage_count: None,
            entropy_threshold: None,
            proactive_window_ms: None,
            enabled: true,
        }
    }
}

/// Global rotation policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RotationPolicy {
    /// Time-based rotation interval (None = disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<i64>,

    /// Minimum entropy threshold for rotation trigger (0.0-1.0).
    /// Lower entropy = more predictable patterns = more need for rotation.
    #[serde(default = "default_entropy_threshold")]
    pub entropy_threshold: f64,

    /// Maximum token age before forced rotation (milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_ms: Option<i64>,

    /// Maximum usage count before rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usage_count: Option<u64>,

    /// Default strategy for rotation.
    #[serde(default)]
    pub default_strategy: RotationStrategy,

    /// Proactive refresh window (milliseconds before expiry).
    /// Tokens within this window will be refreshed proactively.
    #[serde(default = "default_proactive_window_ms")]
    pub proactive_window_ms: i64,

    /// Provider-specific overrides.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_overrides: HashMap<String, ProviderRotationConfig>,

    /// Whether rotation is globally enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Maximum number of samples to keep for entropy calculation.
    #[serde(default = "default_max_samples")]
    pub max_entropy_samples: usize,
}

const fn default_entropy_threshold() -> f64 {
    0.3
}

const fn default_proactive_window_ms() -> i64 {
    600_000 // 10 minutes
}

const fn default_max_samples() -> usize {
    100
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            interval_ms: None,
            entropy_threshold: default_entropy_threshold(),
            max_age_ms: None,
            max_usage_count: None,
            default_strategy: RotationStrategy::default(),
            proactive_window_ms: default_proactive_window_ms(),
            provider_overrides: HashMap::new(),
            enabled: true,
            max_entropy_samples: default_max_samples(),
        }
    }
}

impl RotationPolicy {
    /// Create a new rotation policy with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the effective configuration for a provider.
    ///
    /// Provider-specific overrides take precedence over global defaults.
    pub fn config_for_provider(&self, provider: &str) -> EffectiveRotationConfig {
        let override_config = self.provider_overrides.get(provider);

        EffectiveRotationConfig {
            strategy: override_config.map_or(self.default_strategy, |c| c.strategy),
            interval_ms: override_config
                .and_then(|c| c.interval_ms)
                .or(self.interval_ms),
            max_usage_count: override_config
                .and_then(|c| c.max_usage_count)
                .or(self.max_usage_count),
            entropy_threshold: override_config
                .and_then(|c| c.entropy_threshold)
                .unwrap_or(self.entropy_threshold),
            proactive_window_ms: override_config
                .and_then(|c| c.proactive_window_ms)
                .unwrap_or(self.proactive_window_ms),
            rotate_on_errors: override_config
                .map_or_else(|| vec![401, 403], |c| c.rotate_on_errors.clone()),
            enabled: override_config.map_or(self.enabled, |c| c.enabled),
            max_age_ms: self.max_age_ms,
            max_entropy_samples: self.max_entropy_samples,
        }
    }

    /// Check if rotation should be triggered based on HTTP status code.
    pub fn should_rotate_on_error(&self, provider: &str, status_code: u16) -> bool {
        if !self.enabled {
            return false;
        }
        let config = self.config_for_provider(provider);
        config.enabled && config.rotate_on_errors.contains(&status_code)
    }

    /// Check if rotation should be triggered based on token age.
    pub fn should_rotate_by_age(&self, provider: &str, age_ms: i64) -> bool {
        if !self.enabled {
            return false;
        }
        let config = self.config_for_provider(provider);
        config.enabled && config.max_age_ms.is_some_and(|max| age_ms >= max)
    }

    /// Check if rotation should be triggered based on usage count.
    pub fn should_rotate_by_usage(&self, provider: &str, usage_count: u64) -> bool {
        if !self.enabled {
            return false;
        }
        let config = self.config_for_provider(provider);
        config.enabled && config.max_usage_count.is_some_and(|max| usage_count >= max)
    }

    /// Check if proactive refresh should be triggered.
    pub fn should_refresh_proactively(&self, provider: &str, time_to_expiry_ms: i64) -> bool {
        if !self.enabled {
            return false;
        }
        let config = self.config_for_provider(provider);
        if !config.enabled {
            return false;
        }
        matches!(
            config.strategy,
            RotationStrategy::Proactive | RotationStrategy::Hybrid
        ) && time_to_expiry_ms <= config.proactive_window_ms
            && time_to_expiry_ms > 0
    }

    /// Add a provider-specific override.
    #[must_use]
    pub fn with_provider_override(
        mut self,
        provider: String,
        config: ProviderRotationConfig,
    ) -> Self {
        self.provider_overrides.insert(provider, config);
        self
    }

    /// Set the global interval for time-based rotation.
    #[must_use]
    pub const fn with_interval(mut self, interval_ms: i64) -> Self {
        self.interval_ms = Some(interval_ms);
        self
    }

    /// Set the global entropy threshold.
    #[must_use]
    pub const fn with_entropy_threshold(mut self, threshold: f64) -> Self {
        self.entropy_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Set the global maximum usage count.
    #[must_use]
    pub const fn with_max_usage_count(mut self, count: u64) -> Self {
        self.max_usage_count = Some(count);
        self
    }

    /// Disable rotation globally.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Effective rotation configuration after merging global and provider-specific settings.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRotationConfig {
    /// Rotation strategy.
    pub strategy: RotationStrategy,

    /// Time interval for fixed-interval rotation.
    pub interval_ms: Option<i64>,

    /// Maximum usage count before rotation.
    pub max_usage_count: Option<u64>,

    /// Entropy threshold for rotation.
    pub entropy_threshold: f64,

    /// Proactive refresh window.
    pub proactive_window_ms: i64,

    /// HTTP status codes that trigger rotation.
    pub rotate_on_errors: Vec<u16>,

    /// Whether rotation is enabled.
    pub enabled: bool,

    /// Maximum token age before forced rotation.
    pub max_age_ms: Option<i64>,

    /// Maximum entropy samples to keep.
    pub max_entropy_samples: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy() {
        let policy = RotationPolicy::default();
        assert!(policy.enabled);
        assert_eq!(policy.default_strategy, RotationStrategy::OnExpiry);
        assert_eq!(policy.entropy_threshold, 0.3);
        assert_eq!(policy.proactive_window_ms, 600_000);
        assert!(policy.interval_ms.is_none());
        assert!(policy.max_usage_count.is_none());
    }

    #[test]
    fn test_disabled_policy() {
        let policy = RotationPolicy::disabled();
        assert!(!policy.enabled);
    }

    #[test]
    fn test_builder_pattern() {
        let policy = RotationPolicy::new()
            .with_interval(3_600_000)
            .with_entropy_threshold(0.5)
            .with_max_usage_count(1000);

        assert_eq!(policy.interval_ms, Some(3_600_000));
        assert_eq!(policy.entropy_threshold, 0.5);
        assert_eq!(policy.max_usage_count, Some(1000));
    }

    #[test]
    fn test_entropy_threshold_clamped() {
        let policy = RotationPolicy::new().with_entropy_threshold(1.5);
        assert_eq!(policy.entropy_threshold, 1.0);

        let policy = RotationPolicy::new().with_entropy_threshold(-0.5);
        assert_eq!(policy.entropy_threshold, 0.0);
    }

    #[test]
    fn test_provider_override() {
        let mut policy = RotationPolicy::new();
        policy.provider_overrides.insert(
            "anthropic".to_string(),
            ProviderRotationConfig {
                strategy: RotationStrategy::Hybrid,
                interval_ms: Some(1_800_000),
                max_usage_count: Some(500),
                ..Default::default()
            },
        );

        let config = policy.config_for_provider("anthropic");
        assert_eq!(config.strategy, RotationStrategy::Hybrid);
        assert_eq!(config.interval_ms, Some(1_800_000));
        assert_eq!(config.max_usage_count, Some(500));

        // Other providers use defaults
        let config = policy.config_for_provider("openai");
        assert_eq!(config.strategy, RotationStrategy::OnExpiry);
        assert_eq!(config.interval_ms, None);
    }

    #[test]
    fn test_should_rotate_on_error() {
        let policy = RotationPolicy::default();

        assert!(policy.should_rotate_on_error("anthropic", 401));
        assert!(policy.should_rotate_on_error("anthropic", 403));
        assert!(!policy.should_rotate_on_error("anthropic", 500));

        // Disabled policy
        let disabled = RotationPolicy::disabled();
        assert!(!disabled.should_rotate_on_error("anthropic", 401));
    }

    #[test]
    fn test_should_rotate_by_age() {
        let policy = RotationPolicy::new().with_max_usage_count(100);

        // Age rotation is separate from usage
        assert!(!policy.should_rotate_by_age("anthropic", 3_600_000));

        let policy = RotationPolicy {
            max_age_ms: Some(3_600_000),
            ..Default::default()
        };

        assert!(!policy.should_rotate_by_age("anthropic", 1_800_000));
        assert!(policy.should_rotate_by_age("anthropic", 3_600_000));
        assert!(policy.should_rotate_by_age("anthropic", 7_200_000));
    }

    #[test]
    fn test_should_rotate_by_usage() {
        let policy = RotationPolicy::new().with_max_usage_count(100);

        assert!(!policy.should_rotate_by_usage("anthropic", 50));
        assert!(!policy.should_rotate_by_usage("anthropic", 99));
        assert!(policy.should_rotate_by_usage("anthropic", 100));
        assert!(policy.should_rotate_by_usage("anthropic", 150));
    }

    #[test]
    fn test_proactive_refresh() {
        let policy = RotationPolicy {
            default_strategy: RotationStrategy::Proactive,
            ..Default::default()
        };

        // Within window, positive time to expiry
        assert!(policy.should_refresh_proactively("anthropic", 300_000));
        assert!(policy.should_refresh_proactively("anthropic", 599_999));

        // Outside window
        assert!(!policy.should_refresh_proactively("anthropic", 600_001));

        // Already expired
        assert!(!policy.should_refresh_proactively("anthropic", -1000));
        assert!(!policy.should_refresh_proactively("anthropic", 0));

        // OnExpiry strategy doesn't trigger proactive
        let on_expiry = RotationPolicy::default();
        assert!(!on_expiry.should_refresh_proactively("anthropic", 300_000));
    }

    #[test]
    fn test_hybrid_strategy_proactive() {
        let policy = RotationPolicy {
            default_strategy: RotationStrategy::Hybrid,
            ..Default::default()
        };

        // Hybrid should also trigger proactive
        assert!(policy.should_refresh_proactively("anthropic", 300_000));
    }

    #[test]
    fn test_provider_rotation_config_default() {
        let config = ProviderRotationConfig::default();
        assert_eq!(config.strategy, RotationStrategy::OnExpiry);
        assert_eq!(config.rotate_on_errors, vec![401, 403]);
        assert!(config.enabled);
        assert!(config.interval_ms.is_none());
    }

    #[test]
    fn test_effective_config_provider_disabled() {
        let mut policy = RotationPolicy::default();
        policy.provider_overrides.insert(
            "anthropic".to_string(),
            ProviderRotationConfig {
                enabled: false,
                ..Default::default()
            },
        );

        let config = policy.config_for_provider("anthropic");
        assert!(!config.enabled);

        // Should not trigger rotation when disabled
        assert!(!policy.should_rotate_on_error("anthropic", 401));
    }

    #[test]
    fn test_custom_rotate_on_errors() {
        let mut policy = RotationPolicy::default();
        policy.provider_overrides.insert(
            "anthropic".to_string(),
            ProviderRotationConfig {
                rotate_on_errors: vec![401, 403, 429],
                ..Default::default()
            },
        );

        assert!(policy.should_rotate_on_error("anthropic", 401));
        assert!(policy.should_rotate_on_error("anthropic", 403));
        assert!(policy.should_rotate_on_error("anthropic", 429));
        assert!(!policy.should_rotate_on_error("anthropic", 500));
    }
}
