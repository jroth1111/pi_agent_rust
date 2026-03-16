//! Integration tests for context management features.
//!
//! Tests the interaction between:
//! - History processor chain
//! - Tool output pruning
//! - Turn-based boundaries
//! - Visibility control
//! - Cache control

use pi::context::{
    CacheControlConfig, CacheControlProcessor, FilterProcessor, HistoryProcessor, LastNProcessor,
    MessageMetadata, NoOpProcessor, ProcessorChain, ToolOutputPruner, ToolOutputTag,
    find_turn_boundary, group_into_turns,
};
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::prompt_assembly::{PromptAssemblyInputs, assemble_prompt_plan};
use pi::prompt_plan::PromptSectionKind;
use pi::provider::ToolDef;
use pi::session::SessionMessage;
use serde_json::json;

// =============================================================================
// Test Helpers
// =============================================================================

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

fn make_tool_result(call_id: &str, tool_name: &str, content: &str) -> SessionMessage {
    SessionMessage::ToolResult {
        tool_call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![ContentBlock::Text(TextContent::new(content))],
        details: None,
        is_error: false,
        timestamp: None,
    }
}

/// Create a realistic conversation with multiple turns and tool calls.
fn make_realistic_conversation(turns: usize, tools_per_turn: usize) -> Vec<SessionMessage> {
    let mut messages = Vec::new();

    for turn in 0..turns {
        // User message
        messages.push(make_user_text(&format!("User message for turn {turn}")));

        // Assistant response with tool calls
        messages.push(make_assistant_text(&format!(
            "Assistant response for turn {turn}"
        )));

        // Tool results
        for tool in 0..tools_per_turn {
            let content = format!(
                "Tool result content for turn {turn} tool {tool}\n{}",
                "x".repeat(100) // Make it reasonably sized
            );
            messages.push(make_tool_result(
                &format!("call_{turn}_{tool}"),
                &format!("tool_{tool}"),
                &content,
            ));
        }

        // Final assistant response
        messages.push(make_assistant_text(&format!(
            "Final response for turn {turn}"
        )));
    }

    messages
}

// =============================================================================
// Processor Chain Integration Tests
// =============================================================================

#[test]
fn processor_chain_with_pruner_preserves_turn_structure() {
    // Create a conversation with large tool outputs
    let messages = make_realistic_conversation(5, 3);

    // Create chain with pruner
    let pruner = ToolOutputPruner::with_thresholds(100, 50);
    let chain = ProcessorChain::new().with(pruner);

    let processed = chain.process(messages);

    // Should preserve message count structure (no pruning with small data)
    assert!(!processed.is_empty());

    // Verify turns are still intact
    let turns = group_into_turns(&processed);
    assert_eq!(turns.len(), 5, "Should still have 5 turns after processing");
}

#[test]
fn prompt_assembly_plan_preserves_section_ordering() {
    let plan = assemble_prompt_plan(PromptAssemblyInputs {
        system_prompt: Some("system guidance".to_string()),
        tools: vec![ToolDef {
            name: "read".to_string(),
            description: "Read file contents".to_string(),
            parameters: json!({"type": "object"}),
        }],
        repo_policy: Some("prefer scoped changes".to_string()),
        task_manifest: Some("task: improve prompt efficiency".to_string()),
        retrieval_bundle: None,
        evidence: None,
        fallback_history: vec![],
        fresh_turn: vec![],
    });

    let kinds: Vec<_> = plan.sections.iter().map(|section| section.kind).collect();
    assert_eq!(kinds[0], PromptSectionKind::StaticPrefix);
    assert_eq!(kinds[1], PromptSectionKind::RepoPolicy);
    assert_eq!(kinds[2], PromptSectionKind::TaskManifest);
}

#[test]
fn processor_chain_filter_then_limit() {
    let messages = vec![
        make_user_text("keep me"),
        make_assistant_text("filter me"),
        make_user_text("keep me too"),
        make_assistant_text("filter me too"),
        make_user_text("keep me three"),
        make_assistant_text("filter me three"),
    ];

    // Filter out assistants, then keep last 2
    let chain = ProcessorChain::new()
        .with(FilterProcessor::new(
            |msg| matches!(msg, SessionMessage::Assistant { .. }),
            "filter_assistants",
        ))
        .with(LastNProcessor::new(2));

    let result = chain.process(messages);

    assert_eq!(result.len(), 2);
    // Should have last 2 user messages
    assert!(matches!(result[0], SessionMessage::User { .. }));
    assert!(matches!(result[1], SessionMessage::User { .. }));
}

#[test]
fn processor_chain_order_matters() {
    let messages = vec![
        make_user_text("msg1"),
        make_user_text("msg2"),
        make_user_text("msg3"),
        make_assistant_text("asst1"),
        make_user_text("msg4"),
    ];

    // Limit then filter (filter removes messages where predicate returns true)
    let chain1 = ProcessorChain::new()
        .with(LastNProcessor::new(3))
        .with(FilterProcessor::new(
            |msg| matches!(msg, SessionMessage::User { .. }),
            "filter_users",
        ));

    // Filter then limit
    let chain2 = ProcessorChain::new()
        .with(FilterProcessor::new(
            |msg| matches!(msg, SessionMessage::User { .. }),
            "filter_users",
        ))
        .with(LastNProcessor::new(3));

    let result1 = chain1.process(messages.clone());
    let result2 = chain2.process(messages);

    // chain1: limit to 3 first (msg3, asst1, msg4), then filter removes users -> just asst1
    assert_eq!(result1.len(), 1);
    assert!(matches!(result1[0], SessionMessage::Assistant { .. }));

    // chain2: filter removes users first (leaving asst1), then limit to 3 -> still asst1
    assert_eq!(result2.len(), 1);
    assert!(matches!(result2[0], SessionMessage::Assistant { .. }));
}

// =============================================================================
// Turn Boundary Integration Tests
// =============================================================================

#[test]
fn turn_boundary_respects_processor_chain() {
    let messages = make_realistic_conversation(10, 2);

    // Find boundary with all messages
    let boundary_full = find_turn_boundary(&messages, 100);

    // Apply processor chain that removes some messages
    let chain = ProcessorChain::new().with(LastNProcessor::new(20));
    let processed = chain.process(messages);

    // Find boundary with processed messages
    let boundary_processed = find_turn_boundary(&processed, 100);

    // Boundaries should be different (processed has fewer messages)
    assert_ne!(boundary_full, boundary_processed);

    // Boundary should be a valid index
    assert!(boundary_processed <= processed.len());
}

#[test]
fn turn_grouping_after_pruning() {
    let mut messages = Vec::new();

    // Create a conversation with varying tool output sizes
    for turn in 0..5 {
        messages.push(make_user_text(&format!("Turn {turn} request")));
        messages.push(make_assistant_text(&format!("Turn {turn} thinking...")));

        // Large tool output for early turns
        let large_content = "x".repeat(5000);
        messages.push(make_tool_result(
            &format!("call_{turn}"),
            "read_file",
            if turn < 3 { &large_content } else { "small" },
        ));

        messages.push(make_assistant_text(&format!("Turn {turn} response")));
    }

    // Group into turns before pruning
    let turns_before = group_into_turns(&messages);
    assert_eq!(turns_before.len(), 5);

    // Apply aggressive pruning
    let pruner = ToolOutputPruner::with_thresholds(50, 10);
    let processed = pruner.process(messages);

    // Group into turns after pruning
    let turns_after = group_into_turns(&processed);
    assert_eq!(
        turns_after.len(),
        5,
        "Turn count should be preserved after pruning"
    );
}

// =============================================================================
// Visibility Integration Tests
// =============================================================================

#[test]
fn visibility_metadata_roundtrip() {
    let meta = MessageMetadata::archived();

    // Serialize to JSON
    let json = serde_json::to_string(&meta).unwrap();

    // Deserialize back
    let decoded: MessageMetadata = serde_json::from_str(&json).unwrap();

    assert_eq!(meta, decoded);
    assert!(decoded.is_user_visible());
    assert!(!decoded.is_agent_visible());
}

#[test]
fn visibility_filter_integration() {
    // Create processor that filters non-agent-visible messages
    // Note: This is a conceptual test - in practice, visibility is stored
    // in message extra fields which SessionMessage doesn't expose directly

    let messages = vec![
        make_user_text("visible message"),
        make_assistant_text("response"),
    ];

    // Process with no-op (visibility filtering would happen at a different layer)
    let chain = ProcessorChain::new().with(NoOpProcessor::new());
    let result = chain.process(messages);

    assert_eq!(result.len(), 2);
}

// =============================================================================
// Cache Control Integration Tests
// =============================================================================

#[test]
fn cache_control_processor_integration() {
    let config = CacheControlConfig {
        last_n_messages: 3,
        ..Default::default()
    };
    let processor = CacheControlProcessor::new(config);

    let messages = make_realistic_conversation(5, 1);
    let processed = processor.process(messages);

    // Messages should be returned (cache control adds metadata, doesn't filter)
    assert!(!processed.is_empty());
}

#[test]
fn cache_control_with_processor_chain() {
    let config = CacheControlConfig {
        last_n_messages: 5,
        ..Default::default()
    };

    let chain = ProcessorChain::new()
        .with(CacheControlProcessor::new(config))
        .with(NoOpProcessor::new());

    let messages = make_realistic_conversation(3, 2);
    let processed = chain.process(messages);

    assert!(!processed.is_empty());
}

// =============================================================================
// Long Session Simulation Tests
// =============================================================================

#[test]
fn long_session_processor_chain_performance() {
    // Simulate a 50-turn conversation
    let messages = make_realistic_conversation(50, 3);

    // Create a processor chain
    let chain = ProcessorChain::new()
        .with(ToolOutputPruner::with_thresholds(10000, 5000))
        .with(LastNProcessor::new(100));

    // Process the messages
    let processed = chain.process(messages.clone());

    // Verify processing worked
    assert!(processed.len() <= messages.len());

    // Verify turns are still valid
    let turns = group_into_turns(&processed);
    assert!(!turns.is_empty());
}

#[test]
fn long_session_turn_boundaries() {
    // Create a long conversation
    let messages = make_realistic_conversation(30, 2);

    // Test finding boundaries at various token targets
    for target in [50, 100, 500, 1000] {
        let boundary = find_turn_boundary(&messages, target);
        assert!(
            boundary <= messages.len(),
            "Boundary {boundary} exceeds message count {}",
            messages.len()
        );
    }
}

// =============================================================================
// Edge Cases
// =============================================================================

#[test]
fn empty_message_handling() {
    let messages: Vec<SessionMessage> = Vec::new();

    let chain = ProcessorChain::new()
        .with(ToolOutputPruner::new())
        .with(LastNProcessor::new(10));

    let result = chain.process(messages);
    assert!(result.is_empty());
}

#[test]
fn single_message_handling() {
    let messages = vec![make_user_text("single message")];

    let chain = ProcessorChain::new()
        .with(ToolOutputPruner::new())
        .with(NoOpProcessor::new());

    let result = chain.process(messages);
    assert_eq!(result.len(), 1);
}

#[test]
fn all_tool_results_handling() {
    // Conversation with only tool results (unusual but possible)
    let messages = vec![
        make_tool_result("call1", "tool1", "result1"),
        make_tool_result("call2", "tool2", "result2"),
        make_tool_result("call3", "tool3", "result3"),
    ];

    let pruner = ToolOutputPruner::with_thresholds(10, 5);
    let result = pruner.process(messages);

    // Should still process without error
    assert!(!result.is_empty());
}

#[test]
fn tool_output_tagging_integration() {
    let mut pruner = ToolOutputPruner::with_thresholds(10, 5);

    // Tag one output as essential (never prune)
    pruner.tag("keep_me".to_string(), ToolOutputTag::Essential);

    let large_content = "x".repeat(10000);
    let messages = vec![
        make_user_text("request"),
        make_tool_result("keep_me", "important_tool", &large_content),
        make_tool_result("prune_me", "normal_tool", &large_content),
    ];

    let result = pruner.process(messages);

    // Essential tool result should be preserved
    let mut found_essential = false;
    for msg in &result {
        if let SessionMessage::ToolResult {
            tool_call_id,
            content,
            ..
        } = msg
        {
            if tool_call_id == "keep_me" {
                found_essential = true;
                // Should still have full content
                if let ContentBlock::Text(text) = &content[0] {
                    assert_eq!(
                        text.text.len(),
                        10000,
                        "Essential output should not be pruned"
                    );
                }
            }
        }
    }
    assert!(found_essential, "Should find essential tool result");
}
