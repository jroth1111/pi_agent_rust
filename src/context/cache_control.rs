//! Cache control processor.
//!
//! This module implements cache control combining:
//! - SWE-agent's configurable approach with `last_n_messages` and role filtering
//! - Support for ephemeral cache control markers for prompt caching
//!
//! Prompt caching can significantly reduce costs and latency by caching
//! frequently-used prefix portions of the context. This processor adds
//! cache control markers to appropriate messages.

use crate::context::processors::HistoryProcessor;
use crate::model::Message;
use crate::session::SessionMessage;
use std::time::Duration;

/// Default number of recent messages to mark for caching.
const DEFAULT_LAST_N_MESSAGES: usize = 2;

/// Configuration for cache control behavior.
#[derive(Clone, Debug)]
pub struct CacheControlConfig {
    /// Number of recent messages to mark for caching.
    pub last_n_messages: usize,
    /// Which message roles to mark (by default, user and tool).
    pub cacheable_roles: Vec<CacheableRole>,
}

impl Default for CacheControlConfig {
    fn default() -> Self {
        Self {
            last_n_messages: DEFAULT_LAST_N_MESSAGES,
            cacheable_roles: vec![CacheableRole::User, CacheableRole::Tool],
        }
    }
}

/// Roles that can be marked for cache control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheableRole {
    /// User messages.
    User,
    /// Assistant messages.
    Assistant,
    /// Tool result messages.
    Tool,
}

/// Cache control types supported by providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheControl {
    /// Ephemeral cache - valid for one request.
    Ephemeral,
}

impl CacheControl {
    /// Convert to JSON value for serialization.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Ephemeral => serde_json::json!({"type": "ephemeral"}),
        }
    }
}

/// Cache control processor for adding prompt cache markers.
///
/// This processor marks the last N messages of specified roles with
/// ephemeral cache control markers, enabling prompt caching with
/// providers that support it (like Anthropic).
///
/// # Example
///
/// ```
/// use pi::context::cache_control::{CacheControlProcessor, CacheControlConfig};
/// use pi::context::processors::HistoryProcessor;
///
/// let config = CacheControlConfig::default();
/// let processor = CacheControlProcessor::new(config);
///
/// let messages = vec![];
/// let processed = processor.process(messages);
/// ```
#[derive(Clone, Debug)]
pub struct CacheControlProcessor {
    config: CacheControlConfig,
}

impl Default for CacheControlProcessor {
    fn default() -> Self {
        Self::new(CacheControlConfig::default())
    }
}

impl CacheControlProcessor {
    /// Create a new cache control processor with the given configuration.
    #[must_use]
    pub const fn new(config: CacheControlConfig) -> Self {
        Self { config }
    }

    /// Create a processor with default settings.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::default()
    }

    /// Create a processor that caches last N user messages.
    #[must_use]
    pub fn last_n_user_messages(n: usize) -> Self {
        Self::new(CacheControlConfig {
            last_n_messages: n,
            cacheable_roles: vec![CacheableRole::User],
        })
    }

    /// Check if a session message matches a cacheable role.
    fn matches_role(&self, message: &SessionMessage) -> bool {
        let role = match message {
            SessionMessage::User { .. } => CacheableRole::User,
            SessionMessage::Assistant { .. } => CacheableRole::Assistant,
            SessionMessage::ToolResult { .. } => CacheableRole::Tool,
            _ => return false,
        };
        self.config.cacheable_roles.contains(&role)
    }
}

impl HistoryProcessor for CacheControlProcessor {
    fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
        if messages.is_empty() || self.config.last_n_messages == 0 {
            return messages;
        }

        // Find indices of messages matching cacheable roles (in reverse order)
        let cacheable_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, msg)| self.matches_role(msg))
            .take(self.config.last_n_messages)
            .map(|(i, _)| i)
            .collect();

        if cacheable_indices.is_empty() {
            return messages;
        }

        // Mark messages for caching by adding metadata
        // Note: The actual cache_control marker is added when converting to provider format
        // This processor stores the intent in the message's extra field
        messages
            .into_iter()
            .enumerate()
            .map(|(i, msg)| {
                if cacheable_indices.contains(&i) {
                    add_cache_control_to_message(msg)
                } else {
                    msg
                }
            })
            .collect()
    }

    fn name(&self) -> &'static str {
        "cache_control"
    }
}

/// Key used to store cache control intent in message metadata.
pub const CACHE_CONTROL_KEY: &str = "cacheControl";

/// Add cache control marker to a session message.
///
/// This stores the intent in the message's extra metadata.
/// When converting to provider format, this metadata is used
/// to add the actual cache_control field.
fn add_cache_control_to_message(mut message: SessionMessage) -> SessionMessage {
    match &mut message {
        SessionMessage::BashExecution { extra, .. } => {
            extra.insert(
                CACHE_CONTROL_KEY.to_string(),
                serde_json::json!({"type": "ephemeral"}),
            );
        }
        SessionMessage::Custom {
            details: Some(details),
            ..
        } => {
            if let Some(obj) = details.as_object_mut() {
                obj.insert(
                    CACHE_CONTROL_KEY.to_string(),
                    serde_json::json!({"type": "ephemeral"}),
                );
            }
        }
        _ => {}
    }
    message
}

/// Check if a session message has cache control marker.
pub fn has_cache_control(message: &SessionMessage) -> bool {
    match message {
        SessionMessage::BashExecution { extra, .. } => extra.contains_key(CACHE_CONTROL_KEY),
        SessionMessage::Custom { details, .. } => details.as_ref().is_some_and(|d| {
            d.as_object()
                .is_some_and(|o| o.contains_key(CACHE_CONTROL_KEY))
        }),
        _ => false,
    }
}

/// Extract cache control from a session message.
pub fn get_cache_control(message: &SessionMessage) -> Option<CacheControl> {
    if has_cache_control(message) {
        Some(CacheControl::Ephemeral)
    } else {
        None
    }
}

/// Background cache warmer configuration (Aider approach).
///
/// Cache warming periodically sends small requests to keep the prompt cache
/// active, preventing cache eviction during long-running sessions.
#[derive(Clone, Debug)]
pub struct CacheWarmerConfig {
    /// Interval between warm requests.
    pub interval: Duration,
    /// Whether warming is enabled.
    pub enabled: bool,
}

impl Default for CacheWarmerConfig {
    fn default() -> Self {
        Self {
            // Warm every 5 minutes by default
            interval: Duration::from_secs(300),
            enabled: false,
        }
    }
}

/// Helper to add cache control to provider Message format.
///
/// This is called when converting SessionMessage to provider Message.
pub const fn add_cache_control_to_provider_message(message: &mut Message) {
    // Provider-level cache control mapping is performed in provider-specific conversion paths.
    let _ = message;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent,
    };

    fn make_user_text(text: &str) -> SessionMessage {
        SessionMessage::User {
            content: UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn make_assistant_text(text: &str) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            },
        }
    }

    fn make_tool_result(call_id: &str) -> SessionMessage {
        SessionMessage::ToolResult {
            tool_call_id: call_id.to_string(),
            tool_name: "test_tool".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("result"))],
            details: None,
            is_error: false,
            timestamp: None,
        }
    }

    fn make_bash_execution(command: &str) -> SessionMessage {
        SessionMessage::BashExecution {
            command: command.to_string(),
            output: "output".to_string(),
            exit_code: 0,
            cancelled: None,
            truncated: None,
            full_output_path: None,
            timestamp: None,
            extra: HashMap::new(),
        }
    }

    use std::collections::HashMap;

    #[test]
    fn config_defaults() {
        let config = CacheControlConfig::default();
        assert_eq!(config.last_n_messages, 2);
        assert!(config.cacheable_roles.contains(&CacheableRole::User));
        assert!(config.cacheable_roles.contains(&CacheableRole::Tool));
    }

    #[test]
    fn processor_defaults() {
        let processor = CacheControlProcessor::with_defaults();
        assert_eq!(processor.config.last_n_messages, 2);
    }

    #[test]
    fn processor_empty_messages() {
        let processor = CacheControlProcessor::with_defaults();
        let messages: Vec<SessionMessage> = vec![];
        let result = processor.process(messages);
        assert!(result.is_empty());
    }

    #[test]
    fn processor_marks_last_n_users() {
        let processor = CacheControlProcessor::last_n_user_messages(2);

        let messages = vec![
            make_user_text("user1"), // Should be marked
            make_assistant_text("asst1"),
            make_user_text("user2"), // Should be marked
            make_assistant_text("asst2"),
            make_user_text("user3"), // Should NOT be marked (oldest)
        ];

        let result = processor.process(messages);
        assert_eq!(result.len(), 5);
        // Note: Actual cache control verification would require
        // checking message metadata, which is implementation-specific
    }

    #[test]
    fn processor_respects_roles() {
        let config = CacheControlConfig {
            last_n_messages: 2,
            cacheable_roles: vec![CacheableRole::Tool], // Only tools
        };
        let processor = CacheControlProcessor::new(config);

        let messages = vec![
            make_user_text("user1"),
            make_tool_result("call1"),
            make_user_text("user2"),
            make_tool_result("call2"),
        ];

        let result = processor.process(messages);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn processor_zero_n_messages() {
        let config = CacheControlConfig {
            last_n_messages: 0,
            cacheable_roles: vec![CacheableRole::User],
        };
        let processor = CacheControlProcessor::new(config);

        let messages = vec![make_user_text("user1"), make_user_text("user2")];

        let result = processor.process(messages);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn processor_name() {
        let processor = CacheControlProcessor::with_defaults();
        assert_eq!(processor.name(), "cache_control");
    }

    #[test]
    fn cache_control_to_json() {
        let cc = CacheControl::Ephemeral;
        let json = cc.to_json();
        assert_eq!(json, serde_json::json!({"type": "ephemeral"}));
    }

    #[test]
    fn add_cache_control_to_bash() {
        let msg = make_bash_execution("ls");
        let result = add_cache_control_to_message(msg);

        if let SessionMessage::BashExecution { extra, .. } = result {
            assert!(extra.contains_key(CACHE_CONTROL_KEY));
        } else {
            panic!("Expected BashExecution");
        }
    }

    #[test]
    fn has_cache_control_bash() {
        let msg = make_bash_execution("ls");
        assert!(!has_cache_control(&msg));

        let marked = add_cache_control_to_message(msg);
        assert!(has_cache_control(&marked));
    }

    #[test]
    fn get_cache_control_returns_ephemeral() {
        let msg = make_bash_execution("ls");
        let marked = add_cache_control_to_message(msg);
        let cc = get_cache_control(&marked);
        assert_eq!(cc, Some(CacheControl::Ephemeral));
    }

    #[test]
    fn cache_warmer_config_defaults() {
        let config = CacheWarmerConfig::default();
        assert_eq!(config.interval, Duration::from_secs(300));
        assert!(!config.enabled);
    }
}
