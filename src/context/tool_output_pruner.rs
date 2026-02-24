//! Tool output pruning processor.
//!
//! This module implements tool output pruning combining:
//! - OpenCode's threshold-based approach (PRUNE_MINIMUM/PROTECT constants)
//! - SWE-agent's tagging system for selective pruning
//!
//! Tool outputs can consume significant tokens, especially file read results.
//! This processor walks backwards through tool outputs, keeping recent ones
//! up to a protection threshold and replacing older ones with summaries.

use crate::context::processors::HistoryProcessor;
use crate::model::{ContentBlock, TextContent};
use crate::session::SessionMessage;
use std::collections::HashMap;

/// Minimum tokens that must be saved for pruning to occur (OpenCode: 20,000).
/// If pruning would save less than this, it's not worth the operation.
pub const PRUNE_MINIMUM_TOKENS: u64 = 20_000;

/// Always protect this many tokens of recent tool outputs (OpenCode: 40,000).
/// Recent context is more valuable for task continuity.
pub const PRUNE_PROTECT_TOKENS: u64 = 40_000;

/// Characters per token estimate (matching turns.rs).
const CHARS_PER_TOKEN_ESTIMATE: u64 = 3;

/// Tag for tool outputs that should never be pruned.
pub const TAG_KEEP_OUTPUT: &str = "keep_output";

/// Tag for tool outputs that should always be removed when possible.
pub const TAG_PRUNE_OUTPUT: &str = "prune_output";

/// Tags for selective tool output pruning (SWE-agent approach).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ToolOutputTag {
    /// Never prune this output (e.g., critical context).
    Keep,
    /// Safe to prune when needed (default for most outputs).
    #[default]
    Prune,
    /// Must keep for task continuity (e.g., current file being edited).
    Essential,
}

/// Tool output pruning processor.
///
/// Walks backwards through tool result messages, keeping recent ones
/// up to [`PRUNE_PROTECT_TOKENS`] and replacing older ones with summaries
/// like "[X lines omitted]" to reduce context window usage.
///
/// # Example
///
/// ```
/// use pi::context::tool_output_pruner::ToolOutputPruner;
/// use pi::context::processors::HistoryProcessor;
///
/// let pruner = ToolOutputPruner::new();
/// let messages = vec![];
/// let processed = pruner.process(messages);
/// ```
#[derive(Debug, Clone)]
pub struct ToolOutputPruner {
    /// Tokens of recent tool outputs to always protect.
    protect_tokens: u64,
    /// Minimum savings required to actually prune.
    minimum_savings: u64,
    /// Optional tags for specific tool outputs.
    tags: HashMap<String, ToolOutputTag>,
}

impl Default for ToolOutputPruner {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolOutputPruner {
    /// Create a new tool output pruner with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            protect_tokens: PRUNE_PROTECT_TOKENS,
            minimum_savings: PRUNE_MINIMUM_TOKENS,
            tags: HashMap::new(),
        }
    }

    /// Create a pruner with custom thresholds.
    ///
    /// # Arguments
    /// * `protect_tokens` - Always keep this many tokens of recent outputs
    /// * `minimum_savings` - Only prune if at least this many tokens saved
    #[must_use]
    pub fn with_thresholds(protect_tokens: u64, minimum_savings: u64) -> Self {
        Self {
            protect_tokens,
            minimum_savings,
            tags: HashMap::new(),
        }
    }

    /// Tag a specific tool result for special handling.
    ///
    /// # Arguments
    /// * `tool_call_id` - The tool call ID to tag
    /// * `tag` - The tag to apply
    pub fn tag(&mut self, tool_call_id: String, tag: ToolOutputTag) {
        self.tags.insert(tool_call_id, tag);
    }

    /// Get the tag for a tool result, if any.
    fn get_tag(&self, tool_call_id: &str) -> ToolOutputTag {
        self.tags.get(tool_call_id).copied().unwrap_or_default()
    }

    /// Estimate tokens for a message.
    fn estimate_tokens(message: &SessionMessage) -> u64 {
        let chars = match message {
            SessionMessage::ToolResult { content, .. } => {
                let mut c = 0usize;
                for block in content {
                    if let ContentBlock::Text(text) = block {
                        c += text.text.len();
                    }
                }
                c
            }
            _ => return 0, // Only count tool results
        };
        u64::try_from(chars.div_ceil(CHARS_PER_TOKEN_ESTIMATE as usize)).unwrap_or(0)
    }

    /// Count lines in tool result content.
    fn count_lines(message: &SessionMessage) -> usize {
        if let SessionMessage::ToolResult { content, .. } = message {
            let mut lines = 0;
            for block in content {
                if let ContentBlock::Text(text) = block {
                    lines += text.text.lines().count();
                }
            }
            lines
        } else {
            0
        }
    }

    /// Create a pruned version of a tool result message.
    fn create_pruned_message(&self, original: &SessionMessage) -> SessionMessage {
        match original {
            SessionMessage::ToolResult {
                tool_call_id,
                tool_name,
                details,
                is_error,
                timestamp,
                ..
            } => {
                let lines_omitted = Self::count_lines(original);
                let pruned_content = vec![ContentBlock::Text(TextContent::new(format!(
                    "[{lines_omitted} lines of tool output omitted]"
                )))];
                SessionMessage::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    content: pruned_content,
                    details: details.clone(),
                    is_error: *is_error,
                    timestamp: *timestamp,
                }
            }
            _ => original.clone(),
        }
    }
}

impl HistoryProcessor for ToolOutputPruner {
    fn process(&self, messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
        if messages.is_empty() {
            return messages;
        }

        // First pass: identify tool results and their tokens
        let tool_result_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, SessionMessage::ToolResult { .. }))
            .map(|(i, _)| i)
            .collect();

        if tool_result_indices.is_empty() {
            return messages;
        }

        // Calculate tokens for each tool result and potential savings
        let mut tokens_saved = 0u64;
        let mut recent_tool_tokens = 0u64;
        let mut to_prune: HashMap<usize, bool> = HashMap::new();

        // Walk backwards through tool results
        for &idx in tool_result_indices.iter().rev() {
            let msg = &messages[idx];
            let msg_tokens = Self::estimate_tokens(msg);

            if let SessionMessage::ToolResult { tool_call_id, .. } = msg {
                let tag = self.get_tag(tool_call_id);

                match tag {
                    ToolOutputTag::Keep | ToolOutputTag::Essential => {
                        // Always keep these
                        recent_tool_tokens += msg_tokens;
                    }
                    ToolOutputTag::Prune => {
                        if recent_tool_tokens < self.protect_tokens {
                            // Within protection window, keep it
                            recent_tool_tokens += msg_tokens;
                        } else {
                            // Beyond protection window, mark for pruning
                            let pruned_tokens =
                                Self::estimate_tokens(&self.create_pruned_message(msg));
                            tokens_saved += msg_tokens.saturating_sub(pruned_tokens);
                            to_prune.insert(idx, true);
                        }
                    }
                }
            }
        }

        // Only prune if savings are meaningful
        if tokens_saved < self.minimum_savings {
            return messages;
        }

        // Second pass: apply pruning
        messages
            .into_iter()
            .enumerate()
            .map(|(i, msg)| {
                if to_prune.contains_key(&i) {
                    self.create_pruned_message(&msg)
                } else {
                    msg
                }
            })
            .collect()
    }

    fn name(&self) -> &'static str {
        "tool_output_pruner"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AssistantMessage;
    use crate::model::{StopReason, Usage};

    fn make_tool_result(call_id: &str, content: &str) -> SessionMessage {
        SessionMessage::ToolResult {
            tool_call_id: call_id.to_string(),
            tool_name: "test_tool".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(content))],
            details: None,
            is_error: false,
            timestamp: None,
        }
    }

    fn make_user_text(text: &str) -> SessionMessage {
        SessionMessage::User {
            content: crate::model::UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn make_assistant() -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("ok"))],
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
    fn pruner_empty_messages() {
        let pruner = ToolOutputPruner::new();
        let messages: Vec<SessionMessage> = vec![];
        let result = pruner.process(messages);
        assert!(result.is_empty());
    }

    #[test]
    fn pruner_no_tool_results() {
        let pruner = ToolOutputPruner::new();
        let messages = vec![make_user_text("hello"), make_assistant()];
        let result = pruner.process(messages);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn pruner_single_small_tool_result() {
        let pruner = ToolOutputPruner::with_thresholds(100, 10); // Low thresholds for testing

        // Small tool result - should be kept
        let messages = vec![
            make_user_text("read file"),
            make_assistant(),
            make_tool_result("call_1", "small content"),
        ];

        let result = pruner.process(messages);
        assert_eq!(result.len(), 3);

        // Tool result should be unchanged
        if let SessionMessage::ToolResult { content, .. } = &result[2] {
            if let ContentBlock::Text(text) = &content[0] {
                assert_eq!(text.text, "small content");
            }
        }
    }

    #[test]
    fn pruner_large_tool_result_below_threshold() {
        // Use thresholds that won't trigger pruning
        let pruner = ToolOutputPruner::with_thresholds(1_000_000, 1_000_000);

        let large_content = "x".repeat(1000); // ~333 tokens
        let messages = vec![
            make_user_text("read file"),
            make_assistant(),
            make_tool_result("call_1", &large_content),
        ];

        let result = pruner.process(messages);
        assert_eq!(result.len(), 3);

        // Should NOT be pruned (below minimum savings threshold)
        if let SessionMessage::ToolResult { content, .. } = &result[2] {
            if let ContentBlock::Text(text) = &content[0] {
                assert_eq!(text.text.len(), 1000);
            }
        }
    }

    #[test]
    fn pruner_multiple_tool_results_protection_window() {
        // Small protection window for testing
        let pruner = ToolOutputPruner::with_thresholds(50, 10);

        let content1 = "x".repeat(90); // ~30 tokens
        let content2 = "y".repeat(60); // ~20 tokens
        let content3 = "z".repeat(30); // ~10 tokens

        let messages = vec![
            make_user_text("read"),
            make_assistant(),
            make_tool_result("call_1", &content1),
            make_tool_result("call_2", &content2),
            make_tool_result("call_3", &content3),
        ];

        let result = pruner.process(messages);
        assert_eq!(result.len(), 5);

        // Walking backwards with protection window of 50:
        // call_3 (10 tokens): recent=0 < 50, keep, recent=10
        // call_2 (20 tokens): recent=10 < 50, keep, recent=30
        // call_1 (30 tokens): recent=30 < 50, keep, recent=60
        // Total tokens saved = 0, nothing pruned (protection window keeps all)
        // All results should be kept (unchanged)
        if let SessionMessage::ToolResult {
            content,
            tool_call_id,
            ..
        } = &result[2]
        {
            assert_eq!(tool_call_id, "call_1");
            if let ContentBlock::Text(text) = &content[0] {
                // Content should be unchanged since protection window covers all
                assert_eq!(text.text.len(), 90);
            }
        }
    }

    #[test]
    fn pruner_tag_keep_never_pruned() {
        let mut pruner = ToolOutputPruner::with_thresholds(10, 5); // Very aggressive
        pruner.tag("keep_me".to_string(), ToolOutputTag::Keep);

        let large_content = "x".repeat(1000);
        let messages = vec![make_tool_result("keep_me", &large_content)];

        let result = pruner.process(messages);

        // Should be kept despite being large
        if let SessionMessage::ToolResult { content, .. } = &result[0] {
            if let ContentBlock::Text(text) = &content[0] {
                assert_eq!(text.text.len(), 1000);
            }
        }
    }

    #[test]
    fn pruner_tag_essential_never_pruned() {
        let mut pruner = ToolOutputPruner::with_thresholds(10, 5);
        pruner.tag("essential".to_string(), ToolOutputTag::Essential);

        let large_content = "x".repeat(1000);
        let messages = vec![make_tool_result("essential", &large_content)];

        let result = pruner.process(messages);

        if let SessionMessage::ToolResult { content, .. } = &result[0] {
            if let ContentBlock::Text(text) = &content[0] {
                assert_eq!(text.text.len(), 1000);
            }
        }
    }

    #[test]
    fn pruner_estimate_tokens() {
        let msg = make_tool_result("call", "hello world"); // 11 chars => 4 tokens
        let tokens = ToolOutputPruner::estimate_tokens(&msg);
        assert_eq!(tokens, 4);
    }

    #[test]
    fn pruner_count_lines() {
        let msg = make_tool_result("call", "line1\nline2\nline3");
        let lines = ToolOutputPruner::count_lines(&msg);
        assert_eq!(lines, 3);
    }

    #[test]
    fn pruner_name() {
        let pruner = ToolOutputPruner::new();
        assert_eq!(pruner.name(), "tool_output_pruner");
    }

    #[test]
    fn pruner_preserves_non_tool_messages() {
        let pruner = ToolOutputPruner::with_thresholds(10, 5);

        let messages = vec![
            make_user_text("user message"),
            make_assistant(),
            make_tool_result("call", "result"),
        ];

        let result = pruner.process(messages);

        // Check user message preserved
        assert!(matches!(result[0], SessionMessage::User { .. }));
        // Check assistant preserved
        assert!(matches!(result[1], SessionMessage::Assistant { .. }));
    }
}
