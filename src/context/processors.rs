//! History processor chain infrastructure.
//!
//! This module implements the SWE-agent approach for modular message processing.
//! Processors can be chained together to transform, filter, or annotate messages
//! before they are sent to the LLM.
//!
//! # Example
//!
//! ```
//! use pi::context::processors::{ProcessorChain, HistoryProcessor};
//! use pi::session::SessionMessage;
//!
//! // Create a chain and add processors
//! let chain = ProcessorChain::new()
//!     .with(MyCustomProcessor::new());
//!
//! // Process messages
//! let messages = vec![];
//! let processed = chain.process(messages);
//! ```

use crate::model::ContentBlock;
use crate::session::SessionMessage;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A processor that transforms a list of session messages.
///
/// Processors are applied in sequence to transform, filter, or annotate
/// messages before they are included in the LLM context.
///
/// # Thread Safety
///
/// All processors must be `Send + Sync` to support async processing.
pub trait HistoryProcessor: Send + Sync {
    /// Process a list of messages and return the transformed list.
    ///
    /// Implementations may:
    /// - Filter out messages (return a shorter list)
    /// - Modify message content
    /// - Add metadata to messages
    /// - Reorder messages (though this is uncommon)
    ///
    /// # Arguments
    /// * `messages` - The messages to process (ownership transferred for efficiency)
    ///
    /// # Returns
    /// The processed messages
    fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage>;

    /// Returns the name of this processor for logging/debugging.
    fn name(&self) -> &'static str;
}

/// A chain of history processors applied in sequence.
///
/// Processors are applied in the order they are added via [`ProcessorChain::with`].
/// Each processor receives the output of the previous processor.
///
/// # Example
///
/// ```
/// use pi::context::processors::{ProcessorChain, HistoryProcessor, NoOpProcessor};
///
/// let chain = ProcessorChain::new()
///     .with(NoOpProcessor::new())
///     .with(NoOpProcessor::new());
///
/// let messages = vec![];
/// let processed = chain.process(messages);
/// ```
#[derive(Default)]
pub struct ProcessorChain {
    processors: Vec<Box<dyn HistoryProcessor>>,
}

impl ProcessorChain {
    /// Create a new empty processor chain.
    #[must_use]
    pub fn new() -> Self {
        Self {
            processors: Vec::new(),
        }
    }

    /// Add a processor to the chain.
    ///
    /// Processors are applied in the order they are added.
    ///
    /// # Arguments
    /// * `processor` - The processor to add
    ///
    /// # Returns
    /// Self for method chaining
    #[must_use]
    pub fn with<P: HistoryProcessor + 'static>(mut self, processor: P) -> Self {
        self.processors.push(Box::new(processor));
        self
    }

    /// Add a processor to the chain, returning the processor ID.
    ///
    /// The ID can be used later to remove the processor.
    ///
    /// # Arguments
    /// * `processor` - The processor to add
    ///
    /// # Returns
    /// The index of the added processor
    pub fn add<P: HistoryProcessor + 'static>(&mut self, processor: P) -> usize {
        let id = self.processors.len();
        self.processors.push(Box::new(processor));
        id
    }

    /// Process messages through all processors in the chain.
    ///
    /// Processors are applied in order; each receives the output of the previous.
    ///
    /// # Arguments
    /// * `messages` - The messages to process
    ///
    /// # Returns
    /// The processed messages after all processors have been applied
    pub fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
        self.processors
            .iter()
            .fold(messages, |msgs, processor| processor.process(msgs))
    }

    /// Get the number of processors in the chain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.processors.len()
    }

    /// Check if the chain is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }

    /// Get processor names for logging/debugging.
    pub fn processor_names(&self) -> Vec<&'static str> {
        self.processors.iter().map(|p| p.name()).collect()
    }
}

/// A no-op processor that passes messages through unchanged.
///
/// Useful for testing or as a placeholder.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpProcessor;

impl NoOpProcessor {
    /// Create a new no-op processor.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl HistoryProcessor for NoOpProcessor {
    fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
        messages
    }

    fn name(&self) -> &'static str {
        "noop"
    }
}

/// A processor that filters out messages based on a predicate.
///
/// Messages that match the predicate are removed from the output.
pub struct FilterProcessor<F>
where
    F: Fn(&SessionMessage) -> bool + Send + Sync,
{
    predicate: F,
    name: &'static str,
}

impl<F> FilterProcessor<F>
where
    F: Fn(&SessionMessage) -> bool + Send + Sync,
{
    /// Create a new filter processor.
    ///
    /// # Arguments
    /// * `predicate` - Function returning true for messages to REMOVE
    /// * `name` - Name for logging/debugging
    #[must_use]
    pub const fn new(predicate: F, name: &'static str) -> Self {
        Self { predicate, name }
    }
}

impl<F> HistoryProcessor for FilterProcessor<F>
where
    F: Fn(&SessionMessage) -> bool + Send + Sync,
{
    fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
        messages
            .into_iter()
            .filter(|m| !(self.predicate)(m))
            .collect()
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

/// A processor that limits the number of messages.
///
/// Keeps only the most recent N messages.
pub struct LastNProcessor {
    n: usize,
}

impl LastNProcessor {
    /// Create a processor that keeps only the last N messages.
    ///
    /// # Arguments
    /// * `n` - Maximum number of messages to keep
    #[must_use]
    pub const fn new(n: usize) -> Self {
        Self { n }
    }
}

impl HistoryProcessor for LastNProcessor {
    fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
        if messages.len() <= self.n {
            return messages;
        }
        messages
            .into_iter()
            .rev()
            .take(self.n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    fn name(&self) -> &'static str {
        "last_n"
    }
}

/// Events that must never be removed during compaction (OpenHands approach).
///
/// These event types represent critical state that, if lost, would cause
/// the agent to become confused or repeat actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EssentialEvent {
    /// System messages (tool definitions, rules) that establish context.
    SystemMessage,
    /// First user message (original intent) - the core user request.
    FirstUserMessage,
    /// Action (tool call) awaiting observation - the agent has taken this action
    /// but hasn't seen the result yet. Must be preserved.
    PendingAction { action_id: String, timestamp: u64 },
    /// Observation that must pair with an action - tool result that responds
    /// to a pending action.
    RecallObservation { action_id: String },
}

/// Processor that preserves essential events during compaction.
///
/// This implements the OpenHands approach for preventing context window
/// exhaustion from causing the agent to lose critical state. It ensures:
///
/// 1. System messages are never removed (tools, rules, context)
/// 2. First user message is preserved (original intent)
/// 3. Pending actions (tool calls without results) are kept
/// 4. Observations are paired with their actions (orphaned observations removed)
///
/// # Thread Safety
///
/// This processor is `Send + Sync` but maintains internal state for tracking
/// pending actions. For concurrent use, wrap in appropriate synchronization.
///
/// # Example
///
/// ```
/// use pi::context::processors::EssentialPreservationProcessor;
/// use pi::session::SessionMessage;
///
/// let processor = EssentialPreservationProcessor::new();
/// let messages = vec![];
/// let filtered = processor.process(messages);
/// ```
#[derive(Debug, Clone)]
pub struct EssentialPreservationProcessor {
    /// Essential events to preserve.
    essential: Vec<EssentialEvent>,
    /// Whether we've seen the first user message.
    first_user_seen: bool,
    /// Set of pending action IDs (tool calls awaiting results).
    pending_actions: HashSet<String>,
}

impl Default for EssentialPreservationProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl EssentialPreservationProcessor {
    /// Create a new essential preservation processor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            essential: Vec::new(),
            first_user_seen: false,
            pending_actions: HashSet::new(),
        }
    }

    /// Check if a message is a system message.
    ///
    /// System messages typically contain tool definitions, context setup,
    /// or other information the agent needs to understand the environment.
    fn is_system_message(msg: &SessionMessage) -> bool {
        match msg {
            // Custom messages with specific types are often system/context messages
            SessionMessage::Custom { custom_type, .. } => {
                matches!(
                    custom_type.as_str(),
                    "context" | "system" | "tools" | "capabilities" | "workspace"
                )
            }
            // User messages containing tool definitions or setup
            SessionMessage::User { content, .. } => {
                if let crate::model::UserContent::Text(text) = content {
                    // Heuristic: messages starting with certain prefixes are system-like
                    let text_lower = text.to_lowercase();
                    text_lower.starts_with("system:")
                        || text_lower.starts_with("context:")
                        || text_lower.starts_with("available tools:")
                        || text_lower.starts_with("you have access to")
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Extract action IDs from an assistant message.
    ///
    /// Returns the IDs of all tool calls in the message.
    fn extract_action_ids(msg: &SessionMessage) -> Vec<String> {
        match msg {
            SessionMessage::Assistant { message } => message
                .content
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::ToolCall(call) = block {
                        Some(call.id.clone())
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Check if a tool result message is for a specific action.
    fn is_result_for_action(msg: &SessionMessage, action_id: &str) -> bool {
        matches!(msg, SessionMessage::ToolResult { tool_call_id, .. } if tool_call_id == action_id)
    }

    /// Mark an action as pending (waiting for observation).
    pub fn mark_action_pending(&mut self, action_id: String) {
        self.pending_actions.insert(action_id);
    }

    /// Mark an action as complete (observation received).
    pub fn mark_action_complete(&mut self, action_id: &str) {
        self.pending_actions.remove(action_id);
    }

    /// Get the count of pending actions.
    pub fn pending_count(&self) -> usize {
        self.pending_actions.len()
    }

    /// Reset the processor state (clear pending actions, first user flag).
    pub fn reset(&mut self) {
        self.essential.clear();
        self.first_user_seen = false;
        self.pending_actions.clear();
    }
}

impl HistoryProcessor for EssentialPreservationProcessor {
    fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
        let mut result = Vec::with_capacity(messages.len());
        let mut current_pending = HashSet::new();

        for msg in &messages {
            let keep;

            match msg {
                // Never remove system messages
                SessionMessage::Custom { .. } | SessionMessage::User { .. } => {
                    keep = true;
                }

                // Assistant messages with tool calls - track as pending
                SessionMessage::Assistant { .. } => {
                    let action_ids = Self::extract_action_ids(msg);
                    for id in &action_ids {
                        current_pending.insert(id.clone());
                    }
                    keep = true;
                }

                // Tool results - check if action is pending
                SessionMessage::ToolResult { tool_call_id, .. } => {
                    if current_pending.contains(tool_call_id) {
                        // This result is for a pending action - keep it
                        current_pending.remove(tool_call_id);
                        keep = true;
                    } else if self.pending_actions.contains(tool_call_id) {
                        // This result is for a globally pending action
                        keep = true;
                    } else {
                        // Orphaned observation - action was already compacted away
                        keep = false;
                    }
                }

                // Bash execution - keep as it may contain important state
                SessionMessage::BashExecution { .. } => {
                    keep = true;
                }

                // Preserve all additive session variants by default so compaction
                // cannot silently drop newly introduced structured messages.
                _ => {
                    keep = true;
                }
            }

            if keep {
                result.push(msg.clone());
            }
        }

        result
    }

    fn name(&self) -> &'static str {
        "essential_preservation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent,
    };

    // Helper to create test messages
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

    #[test]
    fn noop_processor_returns_unchanged() {
        let processor = NoOpProcessor::new();
        let messages = vec![make_user_text("hello")];
        let result = processor.process(messages.clone());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn processor_chain_empty() {
        let chain = ProcessorChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);

        let messages = vec![make_user_text("test")];
        let result = chain.process(messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn processor_chain_single() {
        let chain = ProcessorChain::new().with(NoOpProcessor::new());
        assert!(!chain.is_empty());
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.processor_names(), vec!["noop"]);

        let messages = vec![make_user_text("test")];
        let result = chain.process(messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn processor_chain_multiple() {
        let chain = ProcessorChain::new()
            .with(NoOpProcessor::new())
            .with(NoOpProcessor::new());

        assert_eq!(chain.len(), 2);
        assert_eq!(chain.processor_names(), vec!["noop", "noop"]);
    }

    #[test]
    fn filter_processor_removes_matching() {
        let processor = FilterProcessor::new(
            |msg| matches!(msg, SessionMessage::User { .. }),
            "filter_users",
        );

        let messages = vec![
            make_user_text("user1"),
            make_assistant_text("assistant1"),
            make_user_text("user2"),
        ];

        let result = processor.process(messages);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], SessionMessage::Assistant { .. }));
    }

    #[test]
    fn last_n_processor_keeps_recent() {
        let processor = LastNProcessor::new(2);

        let messages = vec![
            make_user_text("old"),
            make_user_text("middle"),
            make_user_text("new"),
        ];

        let result = processor.process(messages);
        assert_eq!(result.len(), 2);

        // Should keep "middle" and "new"
        if let SessionMessage::User { content, .. } = &result[0] {
            if let UserContent::Text(text) = content {
                assert_eq!(text, "middle");
            }
        }
        if let SessionMessage::User { content, .. } = &result[1] {
            if let UserContent::Text(text) = content {
                assert_eq!(text, "new");
            }
        }
    }

    #[test]
    fn last_n_processor_less_than_n() {
        let processor = LastNProcessor::new(10);

        let messages = vec![make_user_text("only")];
        let result = processor.process(messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chain_filter_then_limit() {
        // Filter out assistants, then keep last 1
        let chain = ProcessorChain::new()
            .with(FilterProcessor::new(
                |msg| matches!(msg, SessionMessage::Assistant { .. }),
                "filter_assistants",
            ))
            .with(LastNProcessor::new(1));

        let messages = vec![
            make_user_text("user1"),
            make_assistant_text("asst1"),
            make_user_text("user2"),
            make_assistant_text("asst2"),
            make_user_text("user3"),
        ];

        let result = chain.process(messages);
        assert_eq!(result.len(), 1);

        // Should only have user3
        if let SessionMessage::User { content, .. } = &result[0] {
            if let UserContent::Text(text) = content {
                assert_eq!(text, "user3");
            }
        }
    }

    #[test]
    fn processor_chain_add_returns_id() {
        let mut chain = ProcessorChain::new();
        let id = chain.add(NoOpProcessor::new());
        assert_eq!(id, 0);
        let id2 = chain.add(NoOpProcessor::new());
        assert_eq!(id2, 1);
    }

    // Helper to create custom system messages
    fn make_custom_system(custom_type: &str, content: &str) -> SessionMessage {
        SessionMessage::Custom {
            custom_type: custom_type.to_string(),
            content: content.to_string(),
            display: false,
            details: None,
            timestamp: Some(0),
        }
    }

    // Helper to create assistant with tool call
    fn make_assistant_with_tool(tool_id: &str, tool_name: &str) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::ToolCall(crate::model::ToolCall {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                })],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: 0,
            },
        }
    }

    // Helper to create tool result
    fn make_tool_result(tool_call_id: &str, content: &str) -> SessionMessage {
        SessionMessage::ToolResult {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "test_tool".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(content))],
            details: None,
            is_error: false,
            timestamp: Some(0),
        }
    }

    #[test]
    fn essential_preservation_processor_default() {
        let processor = EssentialPreservationProcessor::new();
        assert!(processor.pending_actions.is_empty());
        assert!(!processor.first_user_seen);
        assert_eq!(processor.name(), "essential_preservation");
    }

    #[test]
    fn essential_preservation_processor_system_messages_kept() {
        let processor = EssentialPreservationProcessor::new();

        let messages = vec![
            make_custom_system("context", "System context"),
            make_custom_system("tools", "Available tools"),
            make_user_text("hello"),
        ];

        let result = processor.process(messages);
        assert_eq!(result.len(), 3); // All kept
    }

    #[test]
    fn essential_preservation_processor_action_observation_pair_kept() {
        let processor = EssentialPreservationProcessor::new();

        let messages = vec![
            make_user_text("run command"),
            make_assistant_with_tool("call_1", "bash"),
            make_tool_result("call_1", "success"),
        ];

        let result = processor.process(messages);
        assert_eq!(result.len(), 3); // All kept (paired correctly)
    }

    #[test]
    fn essential_preservation_processor_orphaned_observation_removed() {
        let processor = EssentialPreservationProcessor::new();

        // Observation without matching action - should be removed
        let messages = vec![
            make_user_text("hello"),
            make_tool_result("orphan_call", "result without action"),
        ];

        let result = processor.process(messages);
        assert_eq!(result.len(), 1); // Only user message kept
        assert!(matches!(result[0], SessionMessage::User { .. }));
    }

    #[test]
    fn essential_preservation_processor_multiple_actions_tracked() {
        let mut processor = EssentialPreservationProcessor::new();

        processor.mark_action_pending("call_1".to_string());
        processor.mark_action_pending("call_2".to_string());
        processor.mark_action_pending("call_3".to_string());

        assert_eq!(processor.pending_count(), 3);

        processor.mark_action_complete("call_1");
        assert_eq!(processor.pending_count(), 2);

        processor.mark_action_complete("call_2");
        processor.mark_action_complete("call_3");
        assert_eq!(processor.pending_count(), 0);
    }

    #[test]
    fn essential_preservation_processor_reset_clears_state() {
        let mut processor = EssentialPreservationProcessor::new();

        processor.mark_action_pending("call_1".to_string());
        assert_eq!(processor.pending_count(), 1);

        processor.reset();
        assert_eq!(processor.pending_count(), 0);
        assert!(processor.essential.is_empty());
    }

    #[test]
    fn essential_preservation_processor_user_system_prefix() {
        let processor = EssentialPreservationProcessor::new();

        let messages = vec![
            make_user_text("System: important context"),
            make_user_text("Context: setup info"),
            make_user_text("Available tools: bash, read"),
            make_user_text("You have access to these tools"),
            make_user_text("regular message"),
        ];

        let result = processor.process(messages);
        // All messages are kept in current implementation
        // System-like detection helps identify important context
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn essential_preservation_processor_bash_execution_kept() {
        let processor = EssentialPreservationProcessor::new();

        let messages = vec![SessionMessage::BashExecution {
            command: "ls -la".to_string(),
            output: "file1.txt\nfile2.txt".to_string(),
            exit_code: 0,
            cancelled: None,
            truncated: None,
            full_output_path: None,
            timestamp: None,
            extra: std::collections::HashMap::new(),
        }];

        let result = processor.process(messages);
        assert_eq!(result.len(), 1);
    }
}
