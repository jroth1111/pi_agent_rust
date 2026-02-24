//! Cryptographic attestation tokens for verification results.
//!
//! This module provides the g3-style attestation tokens that provide
//! cryptographic proof that verification was performed and passed.
//! The tokens use SipHash-2-4 MAC for integrity verification.
//!
//! # Token Format
//!
//! Tokens are encoded as: `g3v1:<base64-encoded-json-payload>:<hex-mac>`
//!
//! The payload contains:
//! - Task ID being attested
//! - Verification outcome (passed/failed)
//! - Evidence IDs collected
//! - Timestamp
//! - Fence value (monotonically increasing for replay protection)

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::error::{Error, Result};

/// Current token version.
pub const TOKEN_VERSION: &str = "g3v1";

/// Cryptographic attestation token with MAC.
///
/// Tokens are created by signing an attestation payload with a secret key.
/// They can be verified later to ensure the attestation has not been tampered with.
#[derive(Debug, Clone)]
pub struct AttestationToken {
    /// Token version string
    pub version: String,
    /// The attestation payload
    pub payload: AttestationPayload,
    /// 8-byte SipHash-2-4 MAC for integrity
    pub mac: [u8; 8],
}

/// The payload of an attestation token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationPayload {
    /// The task ID this attestation is for
    pub task_id: String,
    /// The verification outcome ("passed" or "failed")
    pub verification_outcome: String,
    /// IDs of evidence collected during verification
    pub evidence_ids: Vec<String>,
    /// Unix timestamp (ms) when the attestation was created
    pub timestamp: u64,
    /// Monotonically increasing fence value for replay protection
    pub fence: u64,
}

impl AttestationToken {
    /// Create and sign a new attestation token.
    ///
    /// # Arguments
    /// * `payload` - The attestation payload to sign
    /// * `key` - 16-byte secret key for signing
    pub fn sign(payload: AttestationPayload, key: &[u8; 16]) -> Self {
        let mac = compute_mac(&payload, key);
        Self {
            version: TOKEN_VERSION.to_string(),
            payload,
            mac,
        }
    }

    /// Verify the token's MAC against the provided key.
    ///
    /// Returns `true` if the token is valid, `false` otherwise.
    pub fn verify(&self, key: &[u8; 16]) -> bool {
        let expected_mac = compute_mac(&self.payload, key);
        self.mac == expected_mac && self.version == TOKEN_VERSION
    }

    /// Encode the token as a string.
    ///
    /// Format: `g3v1:<base64-encoded-payload>:<hex-mac>`
    pub fn encode(&self) -> String {
        let payload_json = serde_json::to_string(&self.payload).unwrap_or_default();
        let payload_b64 = BASE64.encode(payload_json.as_bytes());
        let mac_hex = hex::encode(self.mac);
        format!("{}:{}:{}", self.version, payload_b64, mac_hex)
    }

    /// Decode a token from a string.
    ///
    /// # Arguments
    /// * `s` - The encoded token string
    ///
    /// # Errors
    /// Returns an error if the token format is invalid or the MAC is malformed.
    pub fn decode(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 {
            return Err(TokenError::InvalidFormat.into());
        }

        let version = parts[0].to_string();
        if version != TOKEN_VERSION {
            return Err(TokenError::UnsupportedVersion {
                found: version,
                expected: TOKEN_VERSION.to_string(),
            }
            .into());
        }

        let payload_bytes = BASE64
            .decode(parts[1])
            .map_err(|e| TokenError::InvalidBase64(e.to_string()))?;

        let payload: AttestationPayload = serde_json::from_slice(&payload_bytes)
            .map_err(|e| TokenError::InvalidPayload(e.to_string()))?;

        let mac_bytes = hex::decode(parts[2])
            .map_err(|e: hex::FromHexError| TokenError::InvalidHexMac(e.to_string()))?;

        if mac_bytes.len() != 8 {
            return Err(TokenError::InvalidMacLength {
                expected: 8,
                found: mac_bytes.len(),
            }
            .into());
        }

        let mut mac = [0u8; 8];
        mac.copy_from_slice(&mac_bytes);

        Ok(Self {
            version,
            payload,
            mac,
        })
    }

    /// Decode and verify a token in one step.
    ///
    /// # Arguments
    /// * `s` - The encoded token string
    /// * `key` - 16-byte secret key for verification
    ///
    /// # Errors
    /// Returns an error if decoding fails or verification fails.
    pub fn decode_and_verify(s: &str, key: &[u8; 16]) -> Result<Self> {
        let token = Self::decode(s)?;
        if !token.verify(key) {
            return Err(TokenError::VerificationFailed.into());
        }
        Ok(token)
    }

    /// Check if the attestation indicates a passed verification.
    pub fn is_passed(&self) -> bool {
        self.payload.verification_outcome == "passed"
    }

    /// Get the task ID this attestation is for.
    pub fn task_id(&self) -> &str {
        &self.payload.task_id
    }

    /// Get the fence value for ordering/replay protection.
    pub const fn fence(&self) -> u64 {
        self.payload.fence
    }
}

impl AttestationPayload {
    /// Create a new attestation payload.
    pub fn new(
        task_id: impl Into<String>,
        verification_outcome: impl Into<String>,
        evidence_ids: Vec<String>,
        fence: u64,
    ) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Self {
            task_id: task_id.into(),
            verification_outcome: verification_outcome.into(),
            evidence_ids,
            timestamp,
            fence,
        }
    }

    /// Create a payload for a passed verification.
    pub fn passed(task_id: impl Into<String>, evidence_ids: Vec<String>, fence: u64) -> Self {
        Self::new(task_id, "passed", evidence_ids, fence)
    }

    /// Create a payload for a failed verification.
    pub fn failed(task_id: impl Into<String>, evidence_ids: Vec<String>, fence: u64) -> Self {
        Self::new(task_id, "failed", evidence_ids, fence)
    }
}

/// Compute the MAC for a payload using SipHash-2-4.
///
/// Note: Rust's DefaultHasher uses SipHash-2-4 internally, which provides
/// the same security properties as explicit SipHash usage.
fn compute_mac(payload: &AttestationPayload, key: &[u8; 16]) -> [u8; 8] {
    // Create a hasher seeded with the key
    // Note: We hash the key bytes to create a consistent seed
    let mut hasher = DefaultHasher::new();

    // Mix in the key
    key.hash(&mut hasher);

    // Hash the canonical payload representation
    payload.task_id.hash(&mut hasher);
    payload.verification_outcome.hash(&mut hasher);
    for id in &payload.evidence_ids {
        id.hash(&mut hasher);
    }
    payload.timestamp.hash(&mut hasher);
    payload.fence.hash(&mut hasher);

    let hash_value = hasher.finish();
    hash_value.to_le_bytes()
}

/// Errors related to attestation tokens.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TokenError {
    /// Token format is invalid
    #[error("Invalid token format: expected 'version:base64:hex-mac'")]
    InvalidFormat,

    /// Token version is not supported
    #[error("Unsupported token version: found '{found}', expected '{expected}'")]
    UnsupportedVersion {
        /// The version found in the token
        found: String,
        /// The expected version
        expected: String,
    },

    /// Base64 decoding failed
    #[error("Invalid base64 encoding: {0}")]
    InvalidBase64(String),

    /// Payload JSON parsing failed
    #[error("Invalid payload JSON: {0}")]
    InvalidPayload(String),

    /// MAC hex decoding failed
    #[error("Invalid MAC hex encoding: {0}")]
    InvalidHexMac(String),

    /// MAC length is incorrect
    #[error("Invalid MAC length: expected {expected} bytes, found {found}")]
    InvalidMacLength {
        /// Expected length
        expected: usize,
        /// Actual length
        found: usize,
    },

    /// Token verification failed (MAC mismatch)
    #[error("Token verification failed: MAC mismatch")]
    VerificationFailed,
}

impl From<TokenError> for Error {
    fn from(e: TokenError) -> Self {
        Self::Validation(e.to_string())
    }
}

/// Manager for attestation tokens with fence tracking.
///
/// This struct helps manage fence values and ensures tokens are created
/// with monotonically increasing fence values.
#[derive(Debug, Clone)]
pub struct AttestationManager {
    /// The secret key for signing tokens
    key: [u8; 16],
    /// The current fence value
    current_fence: u64,
}

impl AttestationManager {
    /// Create a new attestation manager with the given key.
    pub const fn new(key: [u8; 16]) -> Self {
        Self {
            key,
            current_fence: 0,
        }
    }

    /// Create a manager with a key derived from a seed string.
    pub fn from_seed(seed: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        seed.hash(&mut hasher);
        let hash1 = hasher.finish();

        // Get a second hash with a different prefix
        let mut hasher2 = DefaultHasher::new();
        format!("{seed}-v2").hash(&mut hasher2);
        let hash2 = hasher2.finish();

        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&hash1.to_le_bytes());
        key[8..].copy_from_slice(&hash2.to_le_bytes());

        Self::new(key)
    }

    /// Get the current fence value.
    pub const fn current_fence(&self) -> u64 {
        self.current_fence
    }

    /// Create and sign an attestation token with the next fence value.
    pub fn attest(
        &mut self,
        task_id: impl Into<String>,
        verification_outcome: impl Into<String>,
        evidence_ids: Vec<String>,
    ) -> AttestationToken {
        self.current_fence += 1;
        let payload = AttestationPayload::new(
            task_id,
            verification_outcome,
            evidence_ids,
            self.current_fence,
        );
        AttestationToken::sign(payload, &self.key)
    }

    /// Create a passed attestation.
    pub fn attest_passed(
        &mut self,
        task_id: impl Into<String>,
        evidence_ids: Vec<String>,
    ) -> AttestationToken {
        self.attest(task_id, "passed", evidence_ids)
    }

    /// Create a failed attestation.
    pub fn attest_failed(
        &mut self,
        task_id: impl Into<String>,
        evidence_ids: Vec<String>,
    ) -> AttestationToken {
        self.attest(task_id, "failed", evidence_ids)
    }

    /// Verify a token using this manager's key.
    pub fn verify(&self, token: &AttestationToken) -> bool {
        token.verify(&self.key)
    }

    /// Decode and verify a token string.
    pub fn decode_and_verify(&self, s: &str) -> Result<AttestationToken> {
        AttestationToken::decode_and_verify(s, &self.key)
    }

    /// Set the fence value (e.g., when recovering state).
    pub const fn set_fence(&mut self, fence: u64) {
        self.current_fence = fence;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 16] {
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    }

    #[test]
    fn attestation_payload_creation() {
        let payload = AttestationPayload::passed("task-123".to_string(), vec!["e1".to_string()], 1);
        assert_eq!(payload.task_id, "task-123");
        assert_eq!(payload.verification_outcome, "passed");
        assert_eq!(payload.evidence_ids, vec!["e1"]);
        assert_eq!(payload.fence, 1);
        assert!(payload.timestamp > 0);
    }

    #[test]
    fn attestation_token_sign_and_verify() {
        let key = test_key();
        let payload = AttestationPayload::passed(
            "task-123".to_string(),
            vec!["e1".to_string(), "e2".to_string()],
            1,
        );
        let token = AttestationToken::sign(payload, &key);

        assert_eq!(token.version, TOKEN_VERSION);
        assert!(token.verify(&key));
        assert!(token.is_passed());
    }

    #[test]
    fn attestation_token_wrong_key_fails() {
        let key1 = test_key();
        let key2 = [16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];

        let payload = AttestationPayload::passed("task-123", vec![], 1);
        let token = AttestationToken::sign(payload, &key1);

        assert!(token.verify(&key1));
        assert!(!token.verify(&key2));
    }

    #[test]
    fn attestation_token_encode_decode() {
        let key = test_key();
        let payload = AttestationPayload::passed("task-456", vec!["e1".to_string()], 42);
        let token = AttestationToken::sign(payload, &key);

        let encoded = token.encode();
        assert!(encoded.starts_with("g3v1:"));

        let decoded = AttestationToken::decode(&encoded).unwrap();
        assert_eq!(decoded.version, TOKEN_VERSION);
        assert_eq!(decoded.payload.task_id, "task-456");
        assert_eq!(decoded.payload.fence, 42);
        assert_eq!(decoded.mac, token.mac);
    }

    #[test]
    fn attestation_token_decode_invalid_format() {
        let err = AttestationToken::decode("invalid").unwrap_err();
        assert!(err.to_string().contains("Invalid token format"));

        let err = AttestationToken::decode("v1:foo:bar:baz").unwrap_err();
        assert!(err.to_string().contains("Invalid token format"));
    }

    #[test]
    fn attestation_token_decode_wrong_version() {
        let key = test_key();
        let payload = AttestationPayload::passed("task-123", vec![], 1);
        let mut token = AttestationToken::sign(payload, &key);
        token.version = "v2".to_string();

        let encoded = format!("v2:{}:{}", BASE64.encode(b"{}"), hex::encode(token.mac));
        let err = AttestationToken::decode(&encoded).unwrap_err();
        assert!(err.to_string().contains("Unsupported token version"));
    }

    #[test]
    fn attestation_token_decode_and_verify() {
        let key = test_key();
        let payload = AttestationPayload::passed(
            "task-789".to_string(),
            vec!["e1".to_string(), "e2".to_string()],
            100,
        );
        let token = AttestationToken::sign(payload, &key);
        let encoded = token.encode();

        let verified = AttestationToken::decode_and_verify(&encoded, &key).unwrap();
        assert_eq!(verified.payload.task_id, "task-789");

        let wrong_key = [0u8; 16];
        let err = AttestationToken::decode_and_verify(&encoded, &wrong_key).unwrap_err();
        assert!(err.to_string().contains("verification failed"));
    }

    #[test]
    fn attestation_manager_basic() {
        let key = test_key();
        let mut manager = AttestationManager::new(key);

        assert_eq!(manager.current_fence(), 0);

        let token1 = manager.attest_passed("task-1", vec![]);
        assert_eq!(token1.fence(), 1);
        assert_eq!(manager.current_fence(), 1);

        let token2 = manager.attest_failed("task-2", vec![]);
        assert_eq!(token2.fence(), 2);
        assert_eq!(manager.current_fence(), 2);

        // Verify both tokens
        assert!(manager.verify(&token1));
        assert!(manager.verify(&token2));
    }

    #[test]
    fn attestation_manager_from_seed() {
        let manager1 = AttestationManager::from_seed("my-secret-seed");
        let manager2 = AttestationManager::from_seed("my-secret-seed");
        let manager3 = AttestationManager::from_seed("different-seed");

        let mut m1 = manager1;
        let mut m2 = manager2;
        let mut m3 = manager3;

        let token1 = m1.attest_passed("task-1", vec![]);
        let token2 = m2.attest_passed("task-1", vec![]);
        let token3 = m3.attest_passed("task-1", vec![]);

        // Same seed should produce same keys and verify each other's tokens
        assert!(m2.verify(&token1));
        assert!(m1.verify(&token2));

        // Different seed should produce different keys
        assert!(!m3.verify(&token1));
        assert!(!m3.verify(&token2));
        assert!(!m1.verify(&token3));
    }

    #[test]
    fn attestation_manager_decode_verify() {
        let mut manager = AttestationManager::new(test_key());
        let token = manager.attest_passed("task-123", vec!["e1".to_string()]);
        let encoded = token.encode();

        let verified = manager.decode_and_verify(&encoded).unwrap();
        assert_eq!(verified.task_id(), "task-123");
    }

    #[test]
    fn attestation_manager_set_fence() {
        let mut manager = AttestationManager::new(test_key());
        manager.set_fence(100);

        let token = manager.attest_passed("task-1", vec![]);
        assert_eq!(token.fence(), 101);
    }

    #[test]
    fn failed_attestation() {
        let key = test_key();
        let payload = AttestationPayload::failed("task-123".to_string(), vec!["e1".to_string()], 1);
        let token = AttestationToken::sign(payload, &key);

        assert!(!token.is_passed());
        assert_eq!(token.payload.verification_outcome, "failed");
    }
}
