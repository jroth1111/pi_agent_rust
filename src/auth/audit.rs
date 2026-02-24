//! Lightweight audit logging for OAuth events.
//!
//! This module provides simple in-memory audit logging for authentication
//! events. It's designed for observability without persistence requirements.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Type of audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    /// Token was refreshed.
    TokenRefreshed,

    /// Token was rotated to a different account.
    TokenRotated,

    /// Token was revoked.
    TokenRevoked,

    /// Tokens were cleaned up.
    TokenCleanup,

    /// Account was activated.
    AccountActivated,

    /// Account was deactivated.
    AccountDeactivated,

    /// Recovery process started.
    RecoveryStarted,

    /// Recovery process completed successfully.
    RecoveryCompleted,

    /// Recovery process failed.
    RecoveryFailed,

    /// Entropy threshold triggered rotation.
    EntropyRotationTriggered,

    /// Rate limit encountered.
    RateLimited,

    /// Pool rebalanced.
    PoolRebalanced,
}

/// An audit event record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unix timestamp (milliseconds) when the event occurred.
    pub timestamp_ms: i64,

    /// Type of event.
    pub event_type: AuditEventType,

    /// Provider involved in the event.
    pub provider: String,

    /// Account ID involved (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// Additional details about the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,

    /// Duration of the operation (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl AuditEvent {
    /// Create a new audit event.
    pub const fn new(timestamp_ms: i64, event_type: AuditEventType, provider: String) -> Self {
        Self {
            timestamp_ms,
            event_type,
            provider,
            account_id: None,
            details: None,
            duration_ms: None,
        }
    }

    /// Add an account ID to the event.
    pub fn with_account(mut self, account_id: String) -> Self {
        self.account_id = Some(account_id);
        self
    }

    /// Add details to the event.
    pub fn with_details(mut self, details: String) -> Self {
        self.details = Some(details);
        self
    }

    /// Add a duration to the event.
    pub const fn with_duration(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }
}

/// In-memory audit log.
#[derive(Debug, Default)]
pub struct AuditLog {
    /// Circular buffer of events.
    events: Arc<Mutex<VecDeque<AuditEvent>>>,

    /// Maximum number of events to retain.
    max_events: usize,
}

impl Clone for AuditLog {
    fn clone(&self) -> Self {
        Self {
            events: Arc::clone(&self.events),
            max_events: self.max_events,
        }
    }
}

impl AuditLog {
    /// Create a new audit log with the given capacity.
    pub fn new(max_events: usize) -> Self {
        Self {
            events: Arc::new(Mutex::new(VecDeque::with_capacity(max_events))),
            max_events,
        }
    }

    /// Create an audit log with default capacity (1000 events).
    pub fn with_default_capacity() -> Self {
        Self::new(1000)
    }

    /// Record an audit event.
    pub fn record(&self, event: AuditEvent) {
        let mut events = self.events.lock().expect("audit event log lock");

        // Trim if at capacity
        if events.len() >= self.max_events {
            events.pop_front();
        }

        events.push_back(event);
    }

    /// Record a simple event (no account, details, or duration).
    pub fn record_simple(&self, timestamp_ms: i64, event_type: AuditEventType, provider: &str) {
        self.record(AuditEvent::new(
            timestamp_ms,
            event_type,
            provider.to_string(),
        ));
    }

    /// Get all events (copies).
    pub fn events(&self) -> Vec<AuditEvent> {
        let events = self.events.lock().expect("audit event log lock");
        events.iter().cloned().collect()
    }

    /// Get recent events (most recent first).
    pub fn recent(&self, limit: usize) -> Vec<AuditEvent> {
        let events = self.events.lock().expect("audit event log lock");
        events.iter().rev().take(limit).cloned().collect()
    }

    /// Get events for a specific provider.
    pub fn events_for_provider(&self, provider: &str) -> Vec<AuditEvent> {
        let events = self.events.lock().expect("audit event log lock");
        events
            .iter()
            .filter(|e| e.provider == provider)
            .cloned()
            .collect()
    }

    /// Get events of a specific type.
    pub fn events_of_type(&self, event_type: AuditEventType) -> Vec<AuditEvent> {
        let events = self.events.lock().expect("audit event log lock");
        events
            .iter()
            .filter(|e| e.event_type == event_type)
            .cloned()
            .collect()
    }

    /// Get the number of events in the log.
    pub fn len(&self) -> usize {
        self.events.lock().expect("audit event log lock").len()
    }

    /// Check if the log is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all events from the log.
    pub fn clear(&self) {
        self.events.lock().expect("audit event log lock").clear();
    }

    /// Prune events older than the given timestamp.
    pub fn prune_before(&self, before_ms: i64) -> usize {
        let mut events = self.events.lock().expect("audit event log lock");
        let original_len = events.len();
        events.retain(|e| e.timestamp_ms >= before_ms);
        original_len - events.len()
    }

    /// Get statistics about the audit log.
    pub fn stats(&self) -> AuditStats {
        let events = self.events.lock().expect("audit event log lock");

        let mut by_type: std::collections::HashMap<AuditEventType, usize> =
            std::collections::HashMap::new();
        let mut by_provider: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for event in events.iter() {
            *by_type.entry(event.event_type).or_insert(0) += 1;
            *by_provider.entry(event.provider.clone()).or_insert(0) += 1;
        }

        let oldest = events.front().map(|e| e.timestamp_ms);
        let newest = events.back().map(|e| e.timestamp_ms);

        AuditStats {
            total_events: events.len(),
            by_type,
            by_provider,
            oldest_event_ms: oldest,
            newest_event_ms: newest,
        }
    }
}

/// Statistics about the audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditStats {
    /// Total number of events.
    pub total_events: usize,

    /// Events grouped by type.
    pub by_type: std::collections::HashMap<AuditEventType, usize>,

    /// Events grouped by provider.
    pub by_provider: std::collections::HashMap<String, usize>,

    /// Timestamp of the oldest event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_event_ms: Option<i64>,

    /// Timestamp of the newest event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_event_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_event_creation() {
        let event = AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        );
        assert_eq!(event.timestamp_ms, 1000);
        assert_eq!(event.event_type, AuditEventType::TokenRefreshed);
        assert_eq!(event.provider, "anthropic");
        assert!(event.account_id.is_none());
        assert!(event.details.is_none());
        assert!(event.duration_ms.is_none());
    }

    #[test]
    fn test_audit_event_builder() {
        let event = AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        )
        .with_account("user@example.com".to_string())
        .with_details("Proactive refresh".to_string())
        .with_duration(150);

        assert_eq!(event.account_id, Some("user@example.com".to_string()));
        assert_eq!(event.details, Some("Proactive refresh".to_string()));
        assert_eq!(event.duration_ms, Some(150));
    }

    #[test]
    fn test_audit_log_record() {
        let log = AuditLog::new(100);
        log.record(AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        ));

        assert_eq!(log.len(), 1);
    }

    #[test]
    fn test_audit_log_capacity() {
        let log = AuditLog::new(3);

        for i in 0..5 {
            log.record(AuditEvent::new(
                i * 1000,
                AuditEventType::TokenRefreshed,
                "anthropic".to_string(),
            ));
        }

        assert_eq!(log.len(), 3);

        let events = log.events();
        // Should have the most recent 3
        assert_eq!(events[0].timestamp_ms, 2000);
        assert_eq!(events[1].timestamp_ms, 3000);
        assert_eq!(events[2].timestamp_ms, 4000);
    }

    #[test]
    fn test_audit_log_recent() {
        let log = AuditLog::new(100);

        for i in 0..5 {
            log.record(AuditEvent::new(
                i * 1000,
                AuditEventType::TokenRefreshed,
                "anthropic".to_string(),
            ));
        }

        let recent = log.recent(2);
        assert_eq!(recent.len(), 2);
        // Most recent first
        assert_eq!(recent[0].timestamp_ms, 4000);
        assert_eq!(recent[1].timestamp_ms, 3000);
    }

    #[test]
    fn test_audit_log_by_provider() {
        let log = AuditLog::new(100);

        log.record(AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        ));
        log.record(AuditEvent::new(
            2000,
            AuditEventType::TokenRefreshed,
            "openai".to_string(),
        ));
        log.record(AuditEvent::new(
            3000,
            AuditEventType::TokenRevoked,
            "anthropic".to_string(),
        ));

        let anthropic = log.events_for_provider("anthropic");
        assert_eq!(anthropic.len(), 2);

        let openai = log.events_for_provider("openai");
        assert_eq!(openai.len(), 1);
    }

    #[test]
    fn test_audit_log_by_type() {
        let log = AuditLog::new(100);

        log.record(AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        ));
        log.record(AuditEvent::new(
            2000,
            AuditEventType::TokenRefreshed,
            "openai".to_string(),
        ));
        log.record(AuditEvent::new(
            3000,
            AuditEventType::TokenRevoked,
            "anthropic".to_string(),
        ));

        let refreshed = log.events_of_type(AuditEventType::TokenRefreshed);
        assert_eq!(refreshed.len(), 2);

        let revoked = log.events_of_type(AuditEventType::TokenRevoked);
        assert_eq!(revoked.len(), 1);
    }

    #[test]
    fn test_audit_log_prune() {
        let log = AuditLog::new(100);

        for i in 0..5 {
            log.record(AuditEvent::new(
                i * 1000,
                AuditEventType::TokenRefreshed,
                "anthropic".to_string(),
            ));
        }

        let pruned = log.prune_before(3000);
        assert_eq!(pruned, 3);
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn test_audit_log_clear() {
        let log = AuditLog::new(100);

        log.record(AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        ));
        log.clear();

        assert!(log.is_empty());
    }

    #[test]
    fn test_audit_log_stats() {
        let log = AuditLog::new(100);

        log.record(AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        ));
        log.record(AuditEvent::new(
            2000,
            AuditEventType::TokenRefreshed,
            "openai".to_string(),
        ));
        log.record(AuditEvent::new(
            3000,
            AuditEventType::TokenRevoked,
            "anthropic".to_string(),
        ));

        let stats = log.stats();
        assert_eq!(stats.total_events, 3);
        assert_eq!(
            *stats.by_type.get(&AuditEventType::TokenRefreshed).unwrap(),
            2
        );
        assert_eq!(
            *stats.by_type.get(&AuditEventType::TokenRevoked).unwrap(),
            1
        );
        assert_eq!(*stats.by_provider.get("anthropic").unwrap(), 2);
        assert_eq!(*stats.by_provider.get("openai").unwrap(), 1);
        assert_eq!(stats.oldest_event_ms, Some(1000));
        assert_eq!(stats.newest_event_ms, Some(3000));
    }

    #[test]
    fn test_audit_log_clone() {
        let log1 = AuditLog::new(100);
        log1.record(AuditEvent::new(
            1000,
            AuditEventType::TokenRefreshed,
            "anthropic".to_string(),
        ));

        let log2 = log1.clone();
        assert_eq!(log2.len(), 1);

        // They share the same underlying storage
        log2.record(AuditEvent::new(
            2000,
            AuditEventType::TokenRefreshed,
            "openai".to_string(),
        ));
        assert_eq!(log1.len(), 2);
    }

    #[test]
    fn test_record_simple() {
        let log = AuditLog::new(100);
        log.record_simple(1000, AuditEventType::RecoveryStarted, "anthropic");

        assert_eq!(log.len(), 1);
        let events = log.events();
        assert_eq!(events[0].event_type, AuditEventType::RecoveryStarted);
        assert_eq!(events[0].provider, "anthropic");
    }
}
