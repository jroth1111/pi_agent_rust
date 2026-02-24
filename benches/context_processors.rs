//! Benchmarks for context management processors.
//!
//! Run with: cargo bench -- context
//! Run all: cargo bench
//!
//! Performance targets:
//! - `processor_chain_100_messages`: <10μs
//! - `turn_grouping_100_turns`: <50μs
//! - `tool_pruner_50_results`: <100μs
//! - `find_boundary_100_turns`: <100μs

#[path = "bench_env.rs"]
mod bench_env;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use pi::context::{
    CacheControlConfig, CacheControlProcessor, FilterProcessor, HistoryProcessor, LastNProcessor,
    NoOpProcessor, ProcessorChain, ToolOutputPruner, ToolOutputTag, find_turn_boundary,
    group_into_turns,
};
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::session::SessionMessage;

// =============================================================================
// Test Data Builders
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

/// Build a conversation with the specified number of turns and tools per turn.
fn build_conversation(
    turns: usize,
    tools_per_turn: usize,
    content_size: usize,
) -> Vec<SessionMessage> {
    let mut messages = Vec::with_capacity(turns * (3 + tools_per_turn));
    let content_fill: String = "x".repeat(content_size);

    for turn in 0..turns {
        messages.push(make_user_text(&format!("User request for turn {turn}")));
        messages.push(make_assistant_text(&format!(
            "Assistant response for turn {turn}"
        )));

        for tool in 0..tools_per_turn {
            let content = format!("Tool result {turn}_{tool}: {content_fill}");
            messages.push(make_tool_result(&format!("call_{turn}_{tool}"), &content));
        }

        messages.push(make_assistant_text(&format!(
            "Final response for turn {turn}"
        )));
    }

    messages
}

/// Build messages with only tool results for pruner benchmarks.
fn build_tool_results(count: usize, content_size: usize) -> Vec<SessionMessage> {
    let content_fill: String = "x".repeat(content_size);
    (0..count)
        .map(|i| make_tool_result(&format!("call_{i}"), &format!("Result {i}: {content_fill}")))
        .collect()
}

// =============================================================================
// Processor Chain Benchmarks
// =============================================================================

fn bench_processor_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("processor_chain");

    for msg_count in [10, 50, 100, 500] {
        let messages = build_conversation(msg_count / 4, 2, 100);
        group.throughput(Throughput::Elements(messages.len() as u64));

        // Empty chain
        group.bench_with_input(
            BenchmarkId::new("empty_chain", msg_count),
            &messages,
            |b, messages| {
                let chain = ProcessorChain::new();
                b.iter(|| chain.process(black_box(messages.clone())));
            },
        );

        // Single processor
        group.bench_with_input(
            BenchmarkId::new("single_noop", msg_count),
            &messages,
            |b, messages| {
                let chain = ProcessorChain::new().with(NoOpProcessor::new());
                b.iter(|| chain.process(black_box(messages.clone())));
            },
        );

        // Multiple processors
        group.bench_with_input(
            BenchmarkId::new("multi_processor", msg_count),
            &messages,
            |b, messages| {
                let chain = ProcessorChain::new()
                    .with(NoOpProcessor::new())
                    .with(LastNProcessor::new(50))
                    .with(NoOpProcessor::new());
                b.iter(|| chain.process(black_box(messages.clone())));
            },
        );

        // With filter
        group.bench_with_input(
            BenchmarkId::new("with_filter", msg_count),
            &messages,
            |b, messages| {
                let chain = ProcessorChain::new().with(FilterProcessor::new(
                    |msg| matches!(msg, SessionMessage::Assistant { .. }),
                    "filter_assistants",
                ));
                b.iter(|| chain.process(black_box(messages.clone())));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Turn Grouping Benchmarks
// =============================================================================

fn bench_turn_grouping(c: &mut Criterion) {
    let mut group = c.benchmark_group("turn_grouping");

    for turn_count in [10, 50, 100, 200] {
        let messages = build_conversation(turn_count, 2, 50);
        group.throughput(Throughput::Elements(messages.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("group_into_turns", turn_count),
            &messages,
            |b, messages| {
                b.iter(|| group_into_turns(black_box(messages)));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Turn Boundary Finding Benchmarks
// =============================================================================

fn bench_turn_boundary(c: &mut Criterion) {
    let mut group = c.benchmark_group("turn_boundary");

    for turn_count in [10, 50, 100, 200] {
        let messages = build_conversation(turn_count, 2, 100);
        group.throughput(Throughput::Elements(messages.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("find_boundary_small_target", turn_count),
            &messages,
            |b, messages| {
                b.iter(|| find_turn_boundary(black_box(messages), 100));
            },
        );

        group.bench_with_input(
            BenchmarkId::new("find_boundary_large_target", turn_count),
            &messages,
            |b, messages| {
                b.iter(|| find_turn_boundary(black_box(messages), 10000));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Tool Output Pruner Benchmarks
// =============================================================================

fn bench_tool_pruner(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_pruner");

    for result_count in [10, 50, 100, 200] {
        // Small content - no pruning expected
        let small_content = build_tool_results(result_count, 100);
        group.throughput(Throughput::Elements(small_content.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("small_content", result_count),
            &small_content,
            |b, messages| {
                let pruner = ToolOutputPruner::new();
                b.iter(|| pruner.process(black_box(messages.clone())));
            },
        );

        // Large content - pruning expected
        let large_content = build_tool_results(result_count, 5000);
        group.bench_with_input(
            BenchmarkId::new("large_content", result_count),
            &large_content,
            |b, messages| {
                let pruner = ToolOutputPruner::with_thresholds(1000, 500);
                b.iter(|| pruner.process(black_box(messages.clone())));
            },
        );

        // With tags
        let mut pruner_with_tags = ToolOutputPruner::with_thresholds(1000, 500);
        for i in 0..10 {
            pruner_with_tags.tag(format!("call_{i}"), ToolOutputTag::Essential);
        }
        let tagged_content = build_tool_results(result_count, 5000);
        group.bench_with_input(
            BenchmarkId::new("with_tags", result_count),
            &tagged_content,
            |b, messages| {
                b.iter(|| pruner_with_tags.process(black_box(messages.clone())));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Cache Control Processor Benchmarks
// =============================================================================

fn bench_cache_control(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_control");

    for msg_count in [10, 50, 100, 200] {
        let messages = build_conversation(msg_count / 4, 2, 100);
        group.throughput(Throughput::Elements(messages.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("cache_control", msg_count),
            &messages,
            |b, messages| {
                let config = CacheControlConfig {
                    last_n_messages: 10,
                    ..Default::default()
                };
                let processor = CacheControlProcessor::new(config);
                b.iter(|| processor.process(black_box(messages.clone())));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Full Pipeline Benchmarks
// =============================================================================

fn bench_full_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_pipeline");

    for turn_count in [10, 25, 50, 100] {
        let messages = build_conversation(turn_count, 3, 500);
        group.throughput(Throughput::Elements(messages.len() as u64));

        // Realistic pipeline: pruner -> cache control -> limit
        group.bench_with_input(
            BenchmarkId::new("realistic_pipeline", turn_count),
            &messages,
            |b, messages| {
                let chain = ProcessorChain::new()
                    .with(ToolOutputPruner::with_thresholds(5000, 1000))
                    .with(CacheControlProcessor::new(CacheControlConfig {
                        last_n_messages: 20,
                        ..Default::default()
                    }))
                    .with(LastNProcessor::new(100));
                b.iter(|| chain.process(black_box(messages.clone())));
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_processor_chain,
    bench_turn_grouping,
    bench_turn_boundary,
    bench_tool_pruner,
    bench_cache_control,
    bench_full_pipeline,
);

criterion_main!(benches);
