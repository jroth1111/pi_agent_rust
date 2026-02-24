//! Token revocation and cleanup management.
//!
//! This module provides functionality for tracking revoked tokens and
//! cleaning up expired/orphaned accounts.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub mod cleanup;

/// Reason why a token was revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevocationReason {
    /// User explicitly requested revocation.
    UserRequested,

    /// Token was compromised.
    Compromised,

    /// Authentication failed with this token.
    AuthFailure,

    /// Provider invalidated the token.
    ProviderInvalidated,

    /// Account was removed from configuration.
    AccountRemoved,

    /// Token expired and was not refreshed.
    Expired,

    /// Security policy triggered revocation.
    SecurityPolicy,
}

impl std::fmt::Display for RevocationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserRequested => write!(f, "user requested"),
            Self::Compromised => write!(f, "compromised"),
            Self::AuthFailure => write!(f, "auth failure"),
            Self::ProviderInvalidated => write!(f, "provider invalidated"),
            Self::AccountRemoved => write!(f, "account removed"),
            Self::Expired => write!(f, "expired"),
            Self::SecurityPolicy => write!(f, "security policy"),
        }
    }
}

/// Record of a token revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationRecord {
    /// Hash of the revoked token (SHA-256, hex-encoded).
    pub token_hash: String,

    /// Provider the token belonged to.
    pub provider: String,

    /// Account ID (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// Unix timestamp when revoked.
    pub revoked_at: i64,

    /// Reason for revocation.
    pub reason: RevocationReason,
}

impl RevocationRecord {
    /// Create a new revocation record.
    pub fn new(
        token: &str,
        provider: String,
        account_id: Option<String>,
        revoked_at: i64,
        reason: RevocationReason,
    ) -> Self {
        Self {
            token_hash: hash_token(token),
            provider,
            account_id,
            revoked_at,
            reason,
        }
    }
}

/// Manager for tracking revoked tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevocationManager {
    /// Set of revoked token hashes (SHA-256).
    #[serde(default)]
    revoked: HashSet<String>,

    /// Revocation records for audit purposes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    revocation_log: Vec<RevocationRecord>,

    /// Maximum number of revocation records to keep.
    #[serde(default = "default_max_log_size")]
    max_log_size: usize,
}

const fn default_max_log_size() -> usize {
    1000
}

impl Default for RevocationManager {
    fn default() -> Self {
        Self {
            revoked: HashSet::new(),
            revocation_log: Vec::new(),
            max_log_size: default_max_log_size(),
        }
    }
}

impl RevocationManager {
    /// Create a new revocation manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a revocation manager with a custom log size.
    pub fn with_max_log_size(max_log_size: usize) -> Self {
        Self {
            revoked: HashSet::new(),
            revocation_log: Vec::new(),
            max_log_size,
        }
    }

    /// Revoke a token.
    pub fn revoke(
        &mut self,
        token: &str,
        provider: String,
        account_id: Option<String>,
        revoked_at: i64,
        reason: RevocationReason,
    ) {
        let record = RevocationRecord::new(token, provider, account_id, revoked_at, reason);
        self.revoked.insert(record.token_hash.clone());

        // Add to log, trimming if needed
        self.revocation_log.push(record);
        while self.revocation_log.len() > self.max_log_size {
            self.revocation_log.remove(0);
        }
    }

    /// Check if a token is revoked.
    pub fn is_revoked(&self, token: &str) -> bool {
        let hash = hash_token(token);
        self.revoked.contains(&hash)
    }

    /// Check if a token hash is revoked.
    pub fn is_hash_revoked(&self, hash: &str) -> bool {
        self.revoked.contains(hash)
    }

    /// Remove a token from the revoked set.
    ///
    /// Note: The revocation record is kept for audit purposes.
    pub fn unrevoke(&mut self, token: &str) -> bool {
        let hash = hash_token(token);
        self.revoked.remove(&hash)
    }

    /// Clear all revoked tokens (but keep audit log).
    pub fn clear_revoked(&mut self) {
        self.revoked.clear();
    }

    /// Clear the revocation log.
    pub fn clear_log(&mut self) {
        self.revocation_log.clear();
    }

    /// Clear everything.
    pub fn clear(&mut self) {
        self.revoked.clear();
        self.revocation_log.clear();
    }

    /// Get the number of revoked tokens.
    pub fn revoked_count(&self) -> usize {
        self.revoked.len()
    }

    /// Get the revocation log.
    pub fn revocation_log(&self) -> &[RevocationRecord] {
        &self.revocation_log
    }

    /// Get recent revocations (most recent first).
    pub fn recent_revocations(&self, limit: usize) -> Vec<&RevocationRecord> {
        let start = self.revocation_log.len().saturating_sub(limit);
        self.revocation_log[start..].iter().rev().collect()
    }

    /// Prune old revocation records (older than the given timestamp).
    ///
    /// Note: This only removes from the log, not from the revoked set.
    pub fn prune_log_before(&mut self, before_ms: i64) -> usize {
        let original_len = self.revocation_log.len();
        self.revocation_log.retain(|r| r.revoked_at >= before_ms);
        original_len - self.revocation_log.len()
    }
}

/// Compute SHA-256 hash of a token.
fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

/// Constants for cleanup operations.
pub mod cleanup_constants {
    /// Grace period for expired accounts (7 days).
    pub const CLEANUP_GRACE_PERIOD_MS: i64 = 7 * 24 * 60 * 60 * 1000;

    /// Grace period for accounts requiring relogin (24 hours).
    pub const RELOGIN_GRACE_PERIOD_MS: i64 = 24 * 60 * 60 * 1000;
}

/// Report from a cleanup operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupReport {
    /// Number of expired accounts removed.
    pub expired_accounts_removed: usize,

    /// Number of stale relogin-required accounts removed.
    pub stale_relogin_removed: usize,

    /// Number of orphaned accounts removed.
    pub orphaned_accounts_removed: usize,

    /// Number of duplicate accounts removed.
    pub duplicate_accounts_removed: usize,

    /// Total accounts processed.
    pub accounts_processed: usize,

    /// Timestamp of cleanup.
    pub cleaned_at: i64,

    /// Provider-specific details.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_details: HashMap<String, ProviderCleanupDetail>,
}

impl CleanupReport {
    /// Create a new empty cleanup report.
    pub fn new(cleaned_at: i64) -> Self {
        Self {
            cleaned_at,
            ..Self::default()
        }
    }

    /// Get the total number of accounts removed.
    pub const fn total_removed(&self) -> usize {
        self.expired_accounts_removed
            + self.stale_relogin_removed
            + self.orphaned_accounts_removed
            + self.duplicate_accounts_removed
    }

    /// Check if any cleanup was performed.
    pub const fn is_empty(&self) -> bool {
        self.total_removed() == 0
    }
}

/// Details about cleanup for a specific provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderCleanupDetail {
    /// Accounts before cleanup.
    pub accounts_before: usize,

    /// Accounts after cleanup.
    pub accounts_after: usize,

    /// Reasons for removal.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub removal_reasons: HashMap<String, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_token_consistent() {
        let token = "test_token_123";
        let hash1 = hash_token(token);
        let hash2 = hash_token(token);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_hash_token_different() {
        let hash1 = hash_token("token1");
        let hash2 = hash_token("token2");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_revocation_record_creation() {
        let record = RevocationRecord::new(
            "my_token",
            "anthropic".to_string(),
            Some("user@example.com".to_string()),
            1000,
            RevocationReason::UserRequested,
        );

        assert_eq!(record.provider, "anthropic");
        assert_eq!(record.account_id, Some("user@example.com".to_string()));
        assert_eq!(record.revoked_at, 1000);
        assert_eq!(record.reason, RevocationReason::UserRequested);
        assert_eq!(record.token_hash, hash_token("my_token"));
    }

    #[test]
    fn test_revocation_manager_revoke() {
        let mut manager = RevocationManager::new();
        let token = "test_token";

        manager.revoke(
            token,
            "anthropic".to_string(),
            None,
            1000,
            RevocationReason::AuthFailure,
        );

        assert!(manager.is_revoked(token));
        assert_eq!(manager.revoked_count(), 1);
        assert_eq!(manager.revocation_log().len(), 1);
    }

    #[test]
    fn test_revocation_manager_not_revoked() {
        let manager = RevocationManager::new();
        assert!(!manager.is_revoked("unknown_token"));
    }

    #[test]
    fn test_revocation_manager_unrevoke() {
        let mut manager = RevocationManager::new();
        let token = "test_token";

        manager.revoke(
            token,
            "anthropic".to_string(),
            None,
            1000,
            RevocationReason::UserRequested,
        );
        assert!(manager.is_revoked(token));

        let removed = manager.unrevoke(token);
        assert!(removed);
        assert!(!manager.is_revoked(token));

        // Log is preserved
        assert_eq!(manager.revocation_log().len(), 1);
    }

    #[test]
    fn test_revocation_manager_max_log_size() {
        let mut manager = RevocationManager::with_max_log_size(3);

        for i in 0..5 {
            manager.revoke(
                &format!("token_{i}"),
                "anthropic".to_string(),
                None,
                i as i64 * 1000,
                RevocationReason::Expired,
            );
        }

        assert_eq!(manager.revoked_count(), 5); // All tokens still revoked
        assert_eq!(manager.revocation_log().len(), 3); // Log trimmed
    }

    #[test]
    fn test_revocation_manager_clear() {
        let mut manager = RevocationManager::new();
        manager.revoke(
            "token",
            "anthropic".to_string(),
            None,
            1000,
            RevocationReason::Expired,
        );

        manager.clear();
        assert_eq!(manager.revoked_count(), 0);
        assert_eq!(manager.revocation_log().len(), 0);
    }

    #[test]
    fn test_revocation_manager_prune_log() {
        let mut manager = RevocationManager::new();

        for i in 0..5 {
            manager.revoke(
                &format!("token_{i}"),
                "anthropic".to_string(),
                None,
                i as i64 * 1000,
                RevocationReason::Expired,
            );
        }

        let pruned = manager.prune_log_before(3000);
        assert_eq!(pruned, 3);
        assert_eq!(manager.revocation_log().len(), 2);
    }

    #[test]
    fn test_revocation_manager_recent() {
        let mut manager = RevocationManager::new();

        for i in 0..5 {
            manager.revoke(
                &format!("token_{i}"),
                "anthropic".to_string(),
                None,
                i as i64 * 1000,
                RevocationReason::Expired,
            );
        }

        let recent = manager.recent_revocations(2);
        assert_eq!(recent.len(), 2);
        // Most recent first
        assert_eq!(recent[0].revoked_at, 4000);
        assert_eq!(recent[1].revoked_at, 3000);
    }

    #[test]
    fn test_revocation_reason_display() {
        assert_eq!(
            format!("{}", RevocationReason::UserRequested),
            "user requested"
        );
        assert_eq!(format!("{}", RevocationReason::Compromised), "compromised");
        assert_eq!(format!("{}", RevocationReason::AuthFailure), "auth failure");
    }

    #[test]
    fn test_cleanup_report() {
        let report = CleanupReport::new(1000);
        assert_eq!(report.total_removed(), 0);
        assert!(report.is_empty());

        let report = CleanupReport {
            expired_accounts_removed: 2,
            duplicate_accounts_removed: 1,
            ..CleanupReport::new(1000)
        };
        assert_eq!(report.total_removed(), 3);
        assert!(!report.is_empty());
    }

    #[test]
    fn test_cleanup_constants() {
        // 7 days in milliseconds
        assert_eq!(
            cleanup_constants::CLEANUP_GRACE_PERIOD_MS,
            7 * 24 * 60 * 60 * 1000
        );
        // 24 hours in milliseconds
        assert_eq!(
            cleanup_constants::RELOGIN_GRACE_PERIOD_MS,
            24 * 60 * 60 * 1000
        );
    }
}
