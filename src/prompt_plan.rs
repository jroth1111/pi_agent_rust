use crate::model::{ContentBlock, Message, UserContent};
use crate::provider::ToolDef;

const CHARS_PER_TOKEN_ESTIMATE: u64 = 3;
const IMAGE_TOKEN_ESTIMATE: u64 = 1200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSectionKind {
    StaticPrefix,
    RepoPolicy,
    TaskManifest,
    RetrievalBundle,
    Evidence,
    FreshTurn,
    FallbackHistory,
}

impl PromptSectionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::StaticPrefix => "static_prefix",
            Self::RepoPolicy => "repo_policy",
            Self::TaskManifest => "task_manifest",
            Self::RetrievalBundle => "retrieval_bundle",
            Self::Evidence => "evidence",
            Self::FreshTurn => "fresh_turn",
            Self::FallbackHistory => "fallback_history",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAssemblySection {
    pub kind: PromptSectionKind,
    pub token_estimate: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptTokenBreakdown {
    pub static_prefix: u64,
    pub repo_policy: u64,
    pub task_manifest: u64,
    pub retrieval_bundle: u64,
    pub evidence: u64,
    pub fresh_turn: u64,
    pub fallback_history: u64,
}

impl PromptTokenBreakdown {
    pub const fn total_estimate(&self) -> u64 {
        self.static_prefix
            + self.repo_policy
            + self.task_manifest
            + self.retrieval_bundle
            + self.evidence
            + self.fresh_turn
            + self.fallback_history
    }

    pub fn push_section(&mut self, kind: PromptSectionKind, token_estimate: u64) {
        match kind {
            PromptSectionKind::StaticPrefix => self.static_prefix += token_estimate,
            PromptSectionKind::RepoPolicy => self.repo_policy += token_estimate,
            PromptSectionKind::TaskManifest => self.task_manifest += token_estimate,
            PromptSectionKind::RetrievalBundle => self.retrieval_bundle += token_estimate,
            PromptSectionKind::Evidence => self.evidence += token_estimate,
            PromptSectionKind::FreshTurn => self.fresh_turn += token_estimate,
            PromptSectionKind::FallbackHistory => self.fallback_history += token_estimate,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptAssemblyPlan {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub sections: Vec<PromptAssemblySection>,
    pub token_breakdown: PromptTokenBreakdown,
}

impl PromptAssemblyPlan {
    pub fn add_section(&mut self, kind: PromptSectionKind, token_estimate: u64) {
        self.sections.push(PromptAssemblySection {
            kind,
            token_estimate,
        });
        self.token_breakdown.push_section(kind, token_estimate);
    }
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    chars.div_ceil(CHARS_PER_TOKEN_ESTIMATE)
}

pub fn estimate_content_block_tokens(block: &ContentBlock) -> u64 {
    match block {
        ContentBlock::Text(text) => estimate_text_tokens(&text.text),
        ContentBlock::Thinking(thinking) => estimate_text_tokens(&thinking.thinking),
        ContentBlock::Image(_) => IMAGE_TOKEN_ESTIMATE,
        ContentBlock::ToolCall(tool_call) => {
            estimate_text_tokens(&tool_call.name)
                + estimate_text_tokens(&tool_call.arguments.to_string())
        }
    }
}

pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => estimate_text_tokens(text),
            UserContent::Blocks(blocks) => blocks.iter().map(estimate_content_block_tokens).sum(),
        },
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(estimate_content_block_tokens)
            .sum(),
        Message::ToolResult(result) => result
            .content
            .iter()
            .map(estimate_content_block_tokens)
            .sum(),
        Message::Custom(custom) => estimate_text_tokens(&custom.content),
    }
}

pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

pub fn estimate_tools_tokens(tools: &[ToolDef]) -> u64 {
    tools
        .iter()
        .map(|tool| {
            estimate_text_tokens(&tool.name)
                + estimate_text_tokens(&tool.description)
                + estimate_text_tokens(&tool.parameters.to_string())
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, StopReason, TextContent, ToolResultMessage, Usage};
    use std::sync::Arc;

    #[test]
    fn prompt_token_breakdown_sums_sections() {
        let mut breakdown = PromptTokenBreakdown::default();
        breakdown.push_section(PromptSectionKind::StaticPrefix, 4);
        breakdown.push_section(PromptSectionKind::FallbackHistory, 7);
        breakdown.push_section(PromptSectionKind::FreshTurn, 3);
        assert_eq!(breakdown.total_estimate(), 14);
    }

    #[test]
    fn estimate_messages_counts_images_and_text() {
        let messages = vec![
            Message::User(crate::model::UserMessage {
                content: UserContent::Text("hello world".to_string()),
                timestamp: 0,
            }),
            Message::Assistant(Arc::new(AssistantMessage {
                content: vec![ContentBlock::Image(crate::model::ImageContent {
                    data: "abc".to_string(),
                    mime_type: "image/png".to_string(),
                })],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            })),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "tc1".to_string(),
                tool_name: "read".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("output payload"))],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];

        let estimate = estimate_messages_tokens(&messages);
        assert!(estimate >= IMAGE_TOKEN_ESTIMATE);
    }
}
