//! Message visibility control.
//!
//! Implements the Goose approach to message visibility, allowing messages
//! to be archived from agent context while remaining visible in UI history.

use serde::{Deserialize, Serialize};

/// Key used to store metadata in MessageEntry's extra field.
/// This approach avoids schema changes to SessionMessage variants.
pub const VISIBILITY_METADATA_KEY: &str = "visibilityMetadata";

/// Visibility state for messages within the compaction system.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Copy, Default)]
#[serde(rename_all = "camelCase")]
pub enum VisibilityState {
    /// Message is fully visible to both user and agent
    #[default]
    Visible,
    /// Message is hidden from agent but visible to user (archived from LLM context)
    Hidden,
    /// Message has been replaced with a summary form
    Summary,
}

/// Controls whether a message is visible to the user and/or agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MessageMetadata {
    /// Whether to show in UI history.
    /// When true, the message appears in the conversation view.
    /// When false, the message is hidden from the UI entirely.
    #[serde(default = "default_true")]
    pub user_visible: bool,

    /// Whether to include in LLM context.
    /// When false, the message is "archived" - visible in UI but not sent to the LLM.
    /// This is useful for reducing context window usage while preserving history.
    #[serde(default = "default_true")]
    pub agent_visible: bool,

    /// The visibility state for progressive compaction.
    #[serde(default)]
    pub state: VisibilityState,
}

const fn default_true() -> bool {
    true
}

impl Default for MessageMetadata {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageMetadata {
    /// Create new metadata with default visibility (both visible).
    pub const fn new() -> Self {
        Self {
            user_visible: true,
            agent_visible: true,
            state: VisibilityState::Visible,
        }
    }

    /// Create archived metadata - visible to user but not to agent.
    /// Used when compacting old messages to reduce context window.
    pub const fn archived() -> Self {
        Self {
            user_visible: true,
            agent_visible: false,
            state: VisibilityState::Hidden,
        }
    }

    /// Create hidden metadata - not visible to anyone.
    /// Used for internal system messages.
    pub const fn hidden() -> Self {
        Self {
            user_visible: false,
            agent_visible: false,
            state: VisibilityState::Hidden,
        }
    }

    /// Create summary metadata - message has been summarized.
    pub const fn summary() -> Self {
        Self {
            user_visible: true,
            agent_visible: true,
            state: VisibilityState::Summary,
        }
    }

    /// Check if this message should be included in agent/LLM context.
    pub const fn is_agent_visible(&self) -> bool {
        self.agent_visible
    }

    /// Check if this message should be shown in the UI.
    pub const fn is_user_visible(&self) -> bool {
        self.user_visible
    }

    /// Check if this metadata has default visibility (both true).
    /// Used for skip_serializing_if to avoid adding redundant metadata.
    pub const fn is_default(&self) -> bool {
        self.user_visible && self.agent_visible && matches!(self.state, VisibilityState::Visible)
    }

    /// Get the visibility state.
    pub const fn get_state(&self) -> VisibilityState {
        self.state
    }

    /// Set the visibility state.
    pub const fn set_state(&mut self, state: VisibilityState) {
        self.state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_visibility() {
        let meta = MessageMetadata::default();
        assert!(meta.user_visible);
        assert!(meta.agent_visible);
        assert!(meta.is_agent_visible());
        assert!(meta.is_user_visible());
        assert_eq!(meta.state, VisibilityState::Visible);
    }

    #[test]
    fn test_new_visibility() {
        let meta = MessageMetadata::new();
        assert!(meta.user_visible);
        assert!(meta.agent_visible);
        assert_eq!(meta.state, VisibilityState::Visible);
    }

    #[test]
    fn test_archived_visibility() {
        let meta = MessageMetadata::archived();
        assert!(
            meta.user_visible,
            "archived messages should be user-visible"
        );
        assert!(
            !meta.agent_visible,
            "archived messages should not be agent-visible"
        );
        assert!(!meta.is_agent_visible());
        assert!(meta.is_user_visible());
        assert_eq!(meta.state, VisibilityState::Hidden);
    }

    #[test]
    fn test_hidden_visibility() {
        let meta = MessageMetadata::hidden();
        assert!(!meta.user_visible);
        assert!(!meta.agent_visible);
        assert!(!meta.is_agent_visible());
        assert!(!meta.is_user_visible());
        assert_eq!(meta.state, VisibilityState::Hidden);
    }

    #[test]
    fn test_summary_visibility() {
        let meta = MessageMetadata::summary();
        assert!(meta.user_visible);
        assert!(meta.agent_visible);
        assert!(meta.is_agent_visible());
        assert!(meta.is_user_visible());
        assert_eq!(meta.state, VisibilityState::Summary);
    }

    #[test]
    fn test_visibility_state() {
        let mut meta = MessageMetadata::new();
        assert_eq!(meta.get_state(), VisibilityState::Visible);

        meta.set_state(VisibilityState::Hidden);
        assert_eq!(meta.get_state(), VisibilityState::Hidden);

        meta.set_state(VisibilityState::Summary);
        assert_eq!(meta.get_state(), VisibilityState::Summary);
    }

    #[test]
    fn test_serde_roundtrip() {
        let meta = MessageMetadata::archived();
        let json = serde_json::to_string(&meta).unwrap();
        let decoded: MessageMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, decoded);
    }

    #[test]
    fn test_serde_defaults() {
        // Test that missing fields default to true
        let json = r"{}";
        let meta: MessageMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.user_visible);
        assert!(meta.agent_visible);
    }

    #[test]
    fn test_serde_partial() {
        // Test partial JSON
        let json = r#"{"agentVisible": false}"#;
        let meta: MessageMetadata = serde_json::from_str(json).unwrap();
        assert!(
            meta.user_visible,
            "missing userVisible should default to true"
        );
        assert!(!meta.agent_visible);
    }
}
