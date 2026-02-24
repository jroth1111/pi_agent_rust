//! Turn-based boundaries for context management.
//!
//! This module implements turn-based grouping of session messages, ensuring that
//! context compaction and other operations never split conversations mid-turn.
//! A "turn" represents an atomic user interaction: a user message followed by
//! all assistant responses and tool results until the next user message.

use crate::model::{ContentBlock, UserContent};
use crate::session::SessionMessage;

/// Approximate characters per token for English text with GPT-family tokenizers.
/// Intentionally conservative (overestimates tokens) to avoid exceeding context windows.
/// Set to 3 to safely account for code/symbol-heavy content which is denser than prose.
const CHARS_PER_TOKEN_ESTIMATE: usize = 3;

/// Estimated tokens for an image content block (~1200 tokens).
const IMAGE_TOKEN_ESTIMATE: usize = 1200;

/// Character-equivalent estimate for an image (IMAGE_TOKEN_ESTIMATE * CHARS_PER_TOKEN_ESTIMATE).
const IMAGE_CHAR_ESTIMATE: usize = IMAGE_TOKEN_ESTIMATE * CHARS_PER_TOKEN_ESTIMATE;

/// Represents a single user turn in the conversation.
///
/// A turn is an atomic unit consisting of:
/// - One user message that initiated the turn
/// - Zero or more assistant messages (responses)
/// - Zero or more tool results from the assistant's tool calls
#[derive(Debug, Clone)]
pub struct UserTurn {
    /// The user message that started this turn
    pub user_message: SessionMessage,
    /// Assistant messages in response to the user
    pub assistant_messages: Vec<SessionMessage>,
    /// Tool results from the assistant's tool calls
    pub tool_results: Vec<SessionMessage>,
}

impl UserTurn {
    /// Creates a new UserTurn with the given user message.
    pub const fn new(user_message: SessionMessage) -> Self {
        Self {
            user_message,
            assistant_messages: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    /// Estimates the total token count for this turn.
    ///
    /// Uses the same 3 chars/token estimation as the compaction module.
    /// Sums tokens from the user message, all assistant messages, and tool results.
    pub fn token_count(&self) -> u64 {
        let user_tokens = estimate_tokens(&self.user_message);
        let assistant_tokens: u64 = self.assistant_messages.iter().map(estimate_tokens).sum();
        let tool_tokens: u64 = self.tool_results.iter().map(estimate_tokens).sum();

        user_tokens
            .saturating_add(assistant_tokens)
            .saturating_add(tool_tokens)
    }

    /// Returns the total number of messages in this turn.
    pub fn message_count(&self) -> usize {
        1 + self.assistant_messages.len() + self.tool_results.len()
    }
}

/// Estimates the token count for a single session message.
///
/// Uses character-based estimation (chars / CHARS_PER_TOKEN_ESTIMATE, rounded up).
/// This is consistent with the approach used in the compaction module.
fn estimate_tokens(message: &SessionMessage) -> u64 {
    let mut chars: usize = 0;

    match message {
        SessionMessage::User { content, .. } => match content {
            UserContent::Text(text) => chars = text.len(),
            UserContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text(text) => chars += text.text.len(),
                        ContentBlock::Image(_) => chars += IMAGE_CHAR_ESTIMATE,
                        ContentBlock::Thinking(thinking) => chars += thinking.thinking.len(),
                        ContentBlock::ToolCall(call) => {
                            chars += call.name.len();
                            chars += json_byte_len(&call.arguments);
                        }
                    }
                }
            }
        },
        SessionMessage::Assistant { message } => {
            for block in &message.content {
                match block {
                    ContentBlock::Text(text) => chars += text.text.len(),
                    ContentBlock::Thinking(thinking) => chars += thinking.thinking.len(),
                    ContentBlock::Image(_) => chars += IMAGE_CHAR_ESTIMATE,
                    ContentBlock::ToolCall(call) => {
                        chars += call.name.len();
                        chars += json_byte_len(&call.arguments);
                    }
                }
            }
        }
        SessionMessage::ToolResult { content, .. } => {
            for block in content {
                match block {
                    ContentBlock::Text(text) => chars += text.text.len(),
                    ContentBlock::Thinking(thinking) => chars += thinking.thinking.len(),
                    ContentBlock::Image(_) => chars += IMAGE_CHAR_ESTIMATE,
                    ContentBlock::ToolCall(call) => {
                        chars += call.name.len();
                        chars += json_byte_len(&call.arguments);
                    }
                }
            }
        }
        SessionMessage::Custom { content, .. } => chars = content.len(),
        SessionMessage::BashExecution {
            command, output, ..
        } => chars = command.len() + output.len(),
        SessionMessage::BranchSummary { summary, .. }
        | SessionMessage::CompactionSummary { summary, .. } => chars = summary.len(),
    }

    u64::try_from(chars.div_ceil(CHARS_PER_TOKEN_ESTIMATE)).unwrap_or(u64::MAX)
}

/// Count the serialized JSON byte length of a [`serde_json::Value`] without allocating a `String`.
///
/// Uses `serde_json::to_writer` with a sink that only counts bytes.
fn json_byte_len(value: &serde_json::Value) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Counter(0);
    if serde_json::to_writer(&mut c, value).is_err() {
        // Fallback or partial count on error (e.g. recursion limit)
    }
    c.0
}

/// Groups session messages into atomic user turns.
///
/// A turn starts with a User message and includes all subsequent Assistant
/// messages and ToolResults until the next User message.
///
/// Messages that are not User, Assistant, or ToolResult types are included
/// in the current turn if one exists, otherwise they are skipped.
///
/// # Arguments
/// * `messages` - Slice of session messages to group
///
/// # Returns
/// A vector of `UserTurn` instances, one per user message in chronological order.
pub fn group_into_turns(messages: &[SessionMessage]) -> Vec<UserTurn> {
    let mut turns = Vec::new();
    let mut current_turn: Option<UserTurn> = None;

    for message in messages {
        match message {
            SessionMessage::User { .. } => {
                // Save the current turn if it exists
                if let Some(turn) = current_turn.take() {
                    turns.push(turn);
                }
                // Start a new turn
                current_turn = Some(UserTurn::new(message.clone()));
            }
            SessionMessage::Assistant { .. } => {
                if let Some(ref mut turn) = current_turn {
                    turn.assistant_messages.push(message.clone());
                }
                // If no current turn, skip this message (orphan assistant message)
            }
            SessionMessage::ToolResult { .. } => {
                if let Some(ref mut turn) = current_turn {
                    turn.tool_results.push(message.clone());
                }
                // If no current turn, skip this message (orphan tool result)
            }
            // Other message types (BashExecution, Custom, BranchSummary, CompactionSummary)
            // are not part of turns and are skipped
            _ => {}
        }
    }

    // Don't forget the last turn
    if let Some(turn) = current_turn {
        turns.push(turn);
    }

    turns
}

/// Finds a turn boundary index for cutting context.
///
/// This function groups messages into turns, then walks backwards accumulating
/// token counts until the accumulated tokens exceed or meet the target.
/// This ensures we never cut mid-turn, preserving conversation coherence.
///
/// # Arguments
/// * `messages` - Slice of session messages to analyze
/// * `target_tokens` - The target token count threshold
///
/// # Returns
/// The index into `messages` where the cut should occur. This index points to
/// the first message of the turn that pushed the accumulated tokens over the threshold.
/// Returns 0 if all turns combined don't reach the target.
/// Returns `messages.len()` if messages is empty or target is 0.
pub fn find_turn_boundary(messages: &[SessionMessage], target_tokens: u64) -> usize {
    if messages.is_empty() || target_tokens == 0 {
        return messages.len();
    }

    let turns = group_into_turns(messages);

    if turns.is_empty() {
        return messages.len();
    }

    // Walk backwards through turns, accumulating tokens
    let mut accumulated: u64 = 0;
    let mut cut_turn_index = turns.len(); // Index of the turn where we cut

    for (i, turn) in turns.iter().enumerate().rev() {
        let turn_tokens = turn.token_count();
        accumulated = accumulated.saturating_add(turn_tokens);

        if accumulated >= target_tokens {
            cut_turn_index = i;
            break;
        }

        // Continue accumulating, this turn is below threshold
        cut_turn_index = i;
    }

    // If we never reached the target, return 0 (keep all messages)
    if accumulated < target_tokens {
        return 0;
    }

    // Find the message index that corresponds to the start of cut_turn_index
    let mut turn_count = 0;

    for (i, message) in messages.iter().enumerate() {
        if matches!(message, SessionMessage::User { .. }) {
            if turn_count == cut_turn_index {
                return i;
            }
            turn_count += 1;
        }
    }

    // Fallback: shouldn't reach here if turns are consistent with messages
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage};

    // Helper functions to create test messages
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

    fn make_tool_result(text: &str) -> SessionMessage {
        SessionMessage::ToolResult {
            tool_call_id: "call_1".to_string(),
            tool_name: "test_tool".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
            timestamp: None,
        }
    }

    // =============================================================================
    // estimate_tokens tests
    // =============================================================================

    #[test]
    fn estimate_tokens_user_text() {
        let msg = make_user_text("hello world"); // 11 chars => ceil(11/3) = 4
        assert_eq!(estimate_tokens(&msg), 4);
    }

    #[test]
    fn estimate_tokens_empty_text() {
        let msg = make_user_text(""); // 0 chars => 0
        assert_eq!(estimate_tokens(&msg), 0);
    }

    #[test]
    fn estimate_tokens_assistant_text() {
        let msg = make_assistant_text("hello"); // 5 chars => ceil(5/3) = 2
        assert_eq!(estimate_tokens(&msg), 2);
    }

    #[test]
    fn estimate_tokens_tool_result() {
        let msg = make_tool_result("file contents here"); // 18 chars => ceil(18/3) = 6
        assert_eq!(estimate_tokens(&msg), 6);
    }

    // =============================================================================
    // UserTurn::token_count tests
    // =============================================================================

    #[test]
    fn turn_token_count_single_message() {
        let turn = UserTurn::new(make_user_text("hello")); // 5 chars => 2 tokens
        assert_eq!(turn.token_count(), 2);
    }

    #[test]
    fn turn_token_count_with_assistant() {
        let mut turn = UserTurn::new(make_user_text("hi")); // 2 chars => 1 token
        turn.assistant_messages
            .push(make_assistant_text("hello there")); // 11 chars => 4 tokens
        assert_eq!(turn.token_count(), 5);
    }

    #[test]
    fn turn_token_count_full_turn() {
        let mut turn = UserTurn::new(make_user_text("read file")); // 9 chars => 3 tokens
        turn.assistant_messages.push(make_assistant_text("sure")); // 4 chars => 2 tokens
        turn.tool_results
            .push(make_tool_result("file content here")); // 17 chars => 6 tokens
        assert_eq!(turn.token_count(), 11);
    }

    // =============================================================================
    // group_into_turns tests
    // =============================================================================

    #[test]
    fn group_empty_messages() {
        let messages: Vec<SessionMessage> = Vec::new();
        let turns = group_into_turns(&messages);
        assert!(turns.is_empty());
    }

    #[test]
    fn group_single_turn() {
        let messages = vec![
            make_user_text("hello"),
            make_assistant_text("hi there"),
            make_tool_result("result"),
        ];
        let turns = group_into_turns(&messages);

        assert_eq!(turns.len(), 1);
        assert!(matches!(turns[0].user_message, SessionMessage::User { .. }));
        assert_eq!(turns[0].assistant_messages.len(), 1);
        assert_eq!(turns[0].tool_results.len(), 1);
    }

    #[test]
    fn group_multiple_turns() {
        let messages = vec![
            make_user_text("first"),
            make_assistant_text("response1"),
            make_user_text("second"),
            make_assistant_text("response2"),
            make_tool_result("result2"),
        ];
        let turns = group_into_turns(&messages);

        assert_eq!(turns.len(), 2);

        // First turn
        assert_eq!(turns[0].assistant_messages.len(), 1);
        assert_eq!(turns[0].tool_results.len(), 0);

        // Second turn
        assert_eq!(turns[1].assistant_messages.len(), 1);
        assert_eq!(turns[1].tool_results.len(), 1);
    }

    #[test]
    fn group_orphan_messages_skipped() {
        // Assistant message before any user message should be skipped
        let messages = vec![
            make_assistant_text("orphan"),
            make_user_text("first"),
            make_assistant_text("response"),
        ];
        let turns = group_into_turns(&messages);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].assistant_messages.len(), 1);
    }

    #[test]
    fn group_user_only_turns() {
        let messages = vec![
            make_user_text("first"),
            make_user_text("second"),
            make_user_text("third"),
        ];
        let turns = group_into_turns(&messages);

        assert_eq!(turns.len(), 3);
        for turn in &turns {
            assert!(turn.assistant_messages.is_empty());
            assert!(turn.tool_results.is_empty());
        }
    }

    // =============================================================================
    // find_turn_boundary tests
    // =============================================================================

    #[test]
    fn find_boundary_empty_messages() {
        let messages: Vec<SessionMessage> = Vec::new();
        let index = find_turn_boundary(&messages, 100);
        assert_eq!(index, 0);
    }

    #[test]
    fn find_boundary_zero_target() {
        let messages = vec![make_user_text("hello")];
        let index = find_turn_boundary(&messages, 0);
        assert_eq!(index, 1); // Returns len when target is 0
    }

    #[test]
    fn find_boundary_single_turn_below_target() {
        let messages = vec![
            make_user_text("hi"),         // 2 chars => 1 token
            make_assistant_text("hello"), // 5 chars => 2 tokens
        ];
        // Total: 3 tokens
        let index = find_turn_boundary(&messages, 100);
        assert_eq!(index, 0); // Keep all messages since we can't reach target
    }

    #[test]
    fn find_boundary_multiple_turns() {
        // Create messages where we can test boundary detection
        let messages = vec![
            make_user_text("aaaaaaaaaa"), // 10 chars => 4 tokens
            make_assistant_text("bb"),    // 2 chars => 1 token
            // Turn 1 total: ~5 tokens
            make_user_text("cccccccccc"), // 10 chars => 4 tokens
            make_assistant_text("dd"),    // 2 chars => 1 token
            // Turn 2 total: ~5 tokens
            make_user_text("eeeeeeeeee"), // 10 chars => 4 tokens
            make_assistant_text("ff"),    // 2 chars => 1 token
                                          // Turn 3 total: ~5 tokens
                                          // Grand total: ~15 tokens
        ];

        // Target 6 tokens: should cut at turn 2 (index 2)
        // Walk backwards: Turn 3 (5) < 6, Turn 2 (5) -> accumulated 10 >= 6
        // Cut at Turn 2 (turn index 1) -> message index 2
        let index = find_turn_boundary(&messages, 6);
        assert_eq!(index, 2); // Start of turn 2 (second user message "cccccccccc")
    }

    #[test]
    fn find_boundary_keeps_recent_context() {
        let messages = vec![
            make_user_text("old query"),    // 9 chars => 3 tokens
            make_assistant_text("old ans"), // 7 chars => 3 tokens
            // Turn 1: 6 tokens
            make_user_text("new query"), // 9 chars => 3 tokens
            make_assistant_text("new ans"), // 7 chars => 3 tokens
                                         // Turn 2: 6 tokens
        ];

        // Target 5 tokens: Turn 2 alone (6) >= 5, so cut at turn 2
        let index = find_turn_boundary(&messages, 5);
        assert_eq!(index, 2); // Start of "new query"
    }

    #[test]
    fn find_boundary_exact_match() {
        let messages = vec![
            make_user_text("aaa"),      // 3 chars => 1 token
            make_assistant_text("bbb"), // 3 chars => 1 token
            // Turn 1: 2 tokens
            make_user_text("ccc"), // 3 chars => 1 token
            make_assistant_text("ddd"), // 3 chars => 1 token
                                   // Turn 2: 2 tokens
        ];

        // Target 2 tokens: Turn 2 alone = 2, which is >= 2
        let index = find_turn_boundary(&messages, 2);
        assert_eq!(index, 2); // Start of turn 2
    }

    #[test]
    fn find_boundary_with_tool_results() {
        let messages = vec![
            make_user_text("read"),                     // 4 chars => 2 tokens
            make_tool_result("file contents here abc"), // 20 chars => 7 tokens
            // Turn 1: 9 tokens
            make_user_text("write"), // 5 chars => 2 tokens
            make_assistant_text("done"), // 4 chars => 2 tokens
                                     // Turn 2: 4 tokens
        ];

        // Target 5 tokens: Turn 2 (4) < 5, so we need Turn 1 too
        // Turn 2 (4) + Turn 1 (9) = 13 >= 5
        // But wait, we walk backwards: Turn 2 = 4, not enough
        // Then Turn 1 = 9, accumulated = 13 >= 5
        // Cut at turn 1
        let index = find_turn_boundary(&messages, 5);
        assert_eq!(index, 0); // Keep all since accumulated from turn 2 + turn 1 >= target
    }

    // =============================================================================
    // UserTurn::message_count tests
    // =============================================================================

    #[test]
    fn message_count_single() {
        let turn = UserTurn::new(make_user_text("test"));
        assert_eq!(turn.message_count(), 1);
    }

    #[test]
    fn message_count_with_responses() {
        let mut turn = UserTurn::new(make_user_text("test"));
        turn.assistant_messages.push(make_assistant_text("a"));
        turn.assistant_messages.push(make_assistant_text("b"));
        turn.tool_results.push(make_tool_result("c"));
        assert_eq!(turn.message_count(), 4); // 1 user + 2 assistant + 1 tool
    }
}
