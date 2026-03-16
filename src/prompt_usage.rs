use crate::model::{
    ContentBlock, Message, PromptUsageBreakdown, PromptUsageBucket, PromptUsageCost, Usage,
    UserContent, UserMessage,
};
use crate::provider::{Context, ToolDef};
use crate::session::{COMPACTION_SUMMARY_PREFIX, PROMPT_MANIFEST_CUSTOM_TYPE};

const BUCKET_COUNT: usize = 5;
const STATIC_PREFIX_BUCKET: usize = 0;
const TASK_RUNTIME_MANIFEST_BUCKET: usize = 1;
const FRESH_CONVERSATION_BUCKET: usize = 2;
const TOOL_OUTPUTS_BUCKET: usize = 3;
const COMPACTION_SUMMARY_BUCKET: usize = 4;

pub(crate) fn estimate_tool_def_bytes(tool: &ToolDef) -> usize {
    tool.name.len()
        + tool.description.len()
        + serde_json::to_string(&tool.parameters).map_or(0, |json| json.len())
}

pub(crate) fn estimate_message_bytes(message: &Message) -> usize {
    fn estimate_blocks(blocks: &[ContentBlock]) -> usize {
        blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text.text.len(),
                ContentBlock::Thinking(thinking) => thinking.thinking.len(),
                ContentBlock::Image(image) => image.data.len() + image.mime_type.len(),
                ContentBlock::ToolCall(tool_call) => {
                    tool_call.id.len()
                        + tool_call.name.len()
                        + serde_json::to_string(&tool_call.arguments).map_or(0, |json| json.len())
                }
            })
            .sum()
    }

    match message {
        Message::User(UserMessage { content, .. }) => match content {
            UserContent::Text(text) => text.len(),
            UserContent::Blocks(blocks) => estimate_blocks(blocks),
        },
        Message::Assistant(message) => estimate_blocks(&message.content),
        Message::ToolResult(result) => result.tool_call_id.len() + estimate_blocks(&result.content),
        Message::Custom(custom) => custom.content.len(),
    }
}

pub(crate) fn estimate_context_bytes(context: &Context<'_>) -> usize {
    let system_bytes = context
        .system_prompt
        .as_ref()
        .map_or(0, |prompt| prompt.len());
    let message_bytes = context
        .messages
        .iter()
        .map(estimate_message_bytes)
        .sum::<usize>();
    let tool_bytes = context
        .tools
        .iter()
        .map(estimate_tool_def_bytes)
        .sum::<usize>();
    system_bytes + message_bytes + tool_bytes
}

pub(crate) fn attribute_prompt_usage(context: &Context<'_>, usage: &Usage) -> PromptUsageBreakdown {
    let estimated_bytes = estimate_bucket_bytes(context);
    let input_tokens =
        allocate_proportional_u64(usage.input, &estimated_bytes, FRESH_CONVERSATION_BUCKET);
    let cache_read_tokens =
        allocate_proportional_u64(usage.cache_read, &estimated_bytes, STATIC_PREFIX_BUCKET);
    let cache_write_tokens =
        allocate_proportional_u64(usage.cache_write, &estimated_bytes, STATIC_PREFIX_BUCKET);
    let input_cost = allocate_proportional_f64(usage.cost.input, &input_tokens, usage.input);
    let cache_read_cost =
        allocate_proportional_f64(usage.cost.cache_read, &cache_read_tokens, usage.cache_read);
    let cache_write_cost = allocate_proportional_f64(
        usage.cost.cache_write,
        &cache_write_tokens,
        usage.cache_write,
    );

    PromptUsageBreakdown {
        static_prefix: build_bucket(
            estimated_bytes[STATIC_PREFIX_BUCKET],
            input_tokens[STATIC_PREFIX_BUCKET],
            cache_read_tokens[STATIC_PREFIX_BUCKET],
            cache_write_tokens[STATIC_PREFIX_BUCKET],
            input_cost[STATIC_PREFIX_BUCKET],
            cache_read_cost[STATIC_PREFIX_BUCKET],
            cache_write_cost[STATIC_PREFIX_BUCKET],
        ),
        task_runtime_manifest: build_bucket(
            estimated_bytes[TASK_RUNTIME_MANIFEST_BUCKET],
            input_tokens[TASK_RUNTIME_MANIFEST_BUCKET],
            cache_read_tokens[TASK_RUNTIME_MANIFEST_BUCKET],
            cache_write_tokens[TASK_RUNTIME_MANIFEST_BUCKET],
            input_cost[TASK_RUNTIME_MANIFEST_BUCKET],
            cache_read_cost[TASK_RUNTIME_MANIFEST_BUCKET],
            cache_write_cost[TASK_RUNTIME_MANIFEST_BUCKET],
        ),
        fresh_conversation: build_bucket(
            estimated_bytes[FRESH_CONVERSATION_BUCKET],
            input_tokens[FRESH_CONVERSATION_BUCKET],
            cache_read_tokens[FRESH_CONVERSATION_BUCKET],
            cache_write_tokens[FRESH_CONVERSATION_BUCKET],
            input_cost[FRESH_CONVERSATION_BUCKET],
            cache_read_cost[FRESH_CONVERSATION_BUCKET],
            cache_write_cost[FRESH_CONVERSATION_BUCKET],
        ),
        tool_outputs: build_bucket(
            estimated_bytes[TOOL_OUTPUTS_BUCKET],
            input_tokens[TOOL_OUTPUTS_BUCKET],
            cache_read_tokens[TOOL_OUTPUTS_BUCKET],
            cache_write_tokens[TOOL_OUTPUTS_BUCKET],
            input_cost[TOOL_OUTPUTS_BUCKET],
            cache_read_cost[TOOL_OUTPUTS_BUCKET],
            cache_write_cost[TOOL_OUTPUTS_BUCKET],
        ),
        compaction_summary: build_bucket(
            estimated_bytes[COMPACTION_SUMMARY_BUCKET],
            input_tokens[COMPACTION_SUMMARY_BUCKET],
            cache_read_tokens[COMPACTION_SUMMARY_BUCKET],
            cache_write_tokens[COMPACTION_SUMMARY_BUCKET],
            input_cost[COMPACTION_SUMMARY_BUCKET],
            cache_read_cost[COMPACTION_SUMMARY_BUCKET],
            cache_write_cost[COMPACTION_SUMMARY_BUCKET],
        ),
    }
}

fn estimate_bucket_bytes(context: &Context<'_>) -> [u64; BUCKET_COUNT] {
    let mut buckets = [0u64; BUCKET_COUNT];
    buckets[STATIC_PREFIX_BUCKET] = context
        .system_prompt
        .as_ref()
        .map_or(0, |prompt| u64::try_from(prompt.len()).unwrap_or(u64::MAX))
        + context
            .tools
            .iter()
            .map(|tool| u64::try_from(estimate_tool_def_bytes(tool)).unwrap_or(u64::MAX))
            .sum::<u64>();

    for message in context.messages.iter() {
        let bucket = classify_message_bucket(message);
        buckets[bucket] = buckets[bucket]
            .saturating_add(u64::try_from(estimate_message_bytes(message)).unwrap_or(u64::MAX));
    }

    buckets
}

fn classify_message_bucket(message: &Message) -> usize {
    match message {
        Message::Custom(custom) if custom.custom_type == PROMPT_MANIFEST_CUSTOM_TYPE => {
            TASK_RUNTIME_MANIFEST_BUCKET
        }
        Message::ToolResult(_) => TOOL_OUTPUTS_BUCKET,
        Message::User(user) if is_compaction_summary_message(user) => COMPACTION_SUMMARY_BUCKET,
        _ => FRESH_CONVERSATION_BUCKET,
    }
}

fn is_compaction_summary_message(message: &UserMessage) -> bool {
    match &message.content {
        UserContent::Text(text) => text.starts_with(COMPACTION_SUMMARY_PREFIX),
        UserContent::Blocks(blocks) => blocks.iter().any(|block| {
            matches!(block, ContentBlock::Text(text) if text.text.starts_with(COMPACTION_SUMMARY_PREFIX))
        }),
    }
}

fn allocate_proportional_u64(
    total: u64,
    weights: &[u64; BUCKET_COUNT],
    fallback_bucket: usize,
) -> [u64; BUCKET_COUNT] {
    if total == 0 {
        return [0; BUCKET_COUNT];
    }

    let weight_sum = weights.iter().sum::<u64>();
    if weight_sum == 0 {
        let mut allocated = [0; BUCKET_COUNT];
        allocated[fallback_bucket] = total;
        return allocated;
    }

    let mut allocated = [0u64; BUCKET_COUNT];
    let mut remainders = [(0u64, 0usize); BUCKET_COUNT];
    let mut used = 0u64;
    for (idx, weight) in weights.iter().copied().enumerate() {
        let numerator = u128::from(total) * u128::from(weight);
        let quotient = numerator / u128::from(weight_sum);
        let remainder = numerator % u128::from(weight_sum);
        let value = u64::try_from(quotient).unwrap_or(u64::MAX);
        allocated[idx] = value;
        remainders[idx] = (u64::try_from(remainder).unwrap_or(u64::MAX), idx);
        used = used.saturating_add(value);
    }

    let remaining = total.saturating_sub(used);
    remainders.sort_by(|a, b| b.cmp(a));
    for (_, idx) in remainders
        .iter()
        .copied()
        .cycle()
        .take(usize::try_from(remaining).unwrap_or(0))
    {
        allocated[idx] = allocated[idx].saturating_add(1);
    }

    allocated
}

fn allocate_proportional_f64(
    total: f64,
    weights: &[u64; BUCKET_COUNT],
    weight_sum: u64,
) -> [f64; BUCKET_COUNT] {
    if total == 0.0 || weight_sum == 0 {
        return [0.0; BUCKET_COUNT];
    }

    let mut allocated = [0.0; BUCKET_COUNT];
    for (idx, weight) in weights.iter().copied().enumerate() {
        allocated[idx] = total * (weight as f64 / weight_sum as f64);
    }
    allocated
}

fn build_bucket(
    estimated_bytes: u64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    input_cost: f64,
    cache_read_cost: f64,
    cache_write_cost: f64,
) -> PromptUsageBucket {
    PromptUsageBucket {
        estimated_bytes,
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        total_tokens: input_tokens
            .saturating_add(cache_read_tokens)
            .saturating_add(cache_write_tokens),
        cost: PromptUsageCost {
            input: input_cost,
            cache_read: cache_read_cost,
            cache_write: cache_write_cost,
            total: input_cost + cache_read_cost + cache_write_cost,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, Cost, CustomMessage, StopReason, TextContent, ToolResultMessage,
    };
    use serde_json::json;
    use std::borrow::Cow;
    use std::sync::Arc;

    #[test]
    fn attribute_prompt_usage_buckets_context_sections() {
        let context = Context {
            system_prompt: Some(Cow::Borrowed("system prompt")),
            messages: Cow::Owned(vec![
                Message::Custom(CustomMessage {
                    custom_type: PROMPT_MANIFEST_CUSTOM_TYPE.to_string(),
                    content: "Prompt assembly manifest\n@fields task_runtime".to_string(),
                    display: false,
                    details: None,
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(
                        format!("{COMPACTION_SUMMARY_PREFIX}old summary"),
                    ))]),
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("fresh request".to_string()),
                    timestamp: 0,
                }),
                Message::Assistant(Arc::new(AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new("assistant note"))],
                    api: "test".to_string(),
                    provider: "test".to_string(),
                    model: "test".to_string(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 0,
                })),
                Message::ToolResult(Arc::new(ToolResultMessage {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "grep".to_string(),
                    content: vec![ContentBlock::Text(TextContent::new(
                        "src/lib.rs:1: pub fn demo() {}",
                    ))],
                    details: Some(json!({ "ignored": true })),
                    is_error: false,
                    timestamp: 0,
                })),
            ]),
            tools: Cow::Owned(vec![ToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } }
                }),
            }]),
        };
        let usage = Usage {
            input: 100,
            cache_read: 20,
            cache_write: 10,
            cost: Cost {
                input: 1.0,
                cache_read: 0.2,
                cache_write: 0.4,
                total: 1.6,
                ..Cost::default()
            },
            prompt_breakdown: None,
            ..Usage::default()
        };

        let breakdown = attribute_prompt_usage(&context, &usage);

        assert!(breakdown.static_prefix.estimated_bytes > 0);
        assert!(breakdown.task_runtime_manifest.estimated_bytes > 0);
        assert!(breakdown.fresh_conversation.estimated_bytes > 0);
        assert!(breakdown.tool_outputs.estimated_bytes > 0);
        assert!(breakdown.compaction_summary.estimated_bytes > 0);
        assert_eq!(
            breakdown.static_prefix.input_tokens
                + breakdown.task_runtime_manifest.input_tokens
                + breakdown.fresh_conversation.input_tokens
                + breakdown.tool_outputs.input_tokens
                + breakdown.compaction_summary.input_tokens,
            usage.input
        );
        assert_eq!(
            breakdown.static_prefix.cache_read_tokens
                + breakdown.task_runtime_manifest.cache_read_tokens
                + breakdown.fresh_conversation.cache_read_tokens
                + breakdown.tool_outputs.cache_read_tokens
                + breakdown.compaction_summary.cache_read_tokens,
            usage.cache_read
        );
        assert_eq!(
            breakdown.static_prefix.cache_write_tokens
                + breakdown.task_runtime_manifest.cache_write_tokens
                + breakdown.fresh_conversation.cache_write_tokens
                + breakdown.tool_outputs.cache_write_tokens
                + breakdown.compaction_summary.cache_write_tokens,
            usage.cache_write
        );
    }

    #[test]
    fn attribute_prompt_usage_falls_back_to_fresh_conversation_without_bytes() {
        let context = Context {
            system_prompt: None,
            messages: Cow::Owned(Vec::new()),
            tools: Cow::Owned(Vec::new()),
        };
        let usage = Usage {
            input: 11,
            ..Usage::default()
        };

        let breakdown = attribute_prompt_usage(&context, &usage);

        assert_eq!(breakdown.fresh_conversation.input_tokens, 11);
        assert_eq!(breakdown.static_prefix.input_tokens, 0);
        assert_eq!(breakdown.tool_outputs.input_tokens, 0);
    }
}
