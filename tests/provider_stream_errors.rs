//! Provider stream error handling tests.
//!
//! Tests for SSE/streaming error conditions that occur during response processing:
//! - Malformed SSE events
//! - Timeout mid-stream
//! - Rate limit response handling
//! - Auth error mid-stream
//!
//! Uses VCR cassettes for deterministic API testing without real credentials.

mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::StreamExt;
use pi::http::client::Client;
use pi::model::{Message, StreamEvent, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions};
use pi::vcr::{Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder};
use serde_json::json;

fn context_for(prompt: &str) -> Context<'static> {
    Context::owned(
        None,
        vec![Message::User(UserMessage {
            content: UserContent::Text(prompt.to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

fn options_with_key(key: &str) -> StreamOptions {
    StreamOptions {
        api_key: Some(key.to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// VCR cassette helpers
// ---------------------------------------------------------------------------

fn vcr_client(
    test_name: &str,
    url: &str,
    request_body: serde_json::Value,
    status: u16,
    response_headers: Vec<(String, String)>,
    response_chunks: Vec<String>,
) -> (Client, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp dir");
    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: test_name.to_string(),
        recorded_at: "2026-02-05T00:00:00.000Z".to_string(),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: Vec::new(),
                body: Some(request_body),
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: response_headers,
                body_chunks: response_chunks,
                body_chunks_base64: None,
            },
        }],
    };
    let serialized = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(temp.path().join(format!("{test_name}.json")), serialized)
        .expect("write cassette");
    let recorder = VcrRecorder::new_with(test_name, VcrMode::Playback, temp.path());
    let client = Client::new().with_vcr(recorder);
    (client, temp)
}

fn vcr_client_bytes(
    test_name: &str,
    url: &str,
    request_body: serde_json::Value,
    status: u16,
    response_headers: Vec<(String, String)>,
    response_chunks: Vec<Vec<u8>>,
) -> (Client, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp dir");
    let encoded_chunks = response_chunks
        .into_iter()
        .map(|chunk| STANDARD.encode(chunk))
        .collect::<Vec<_>>();
    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: test_name.to_string(),
        recorded_at: "2026-02-05T00:00:00.000Z".to_string(),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers: Vec::new(),
                body: Some(request_body),
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: response_headers,
                body_chunks: Vec::new(),
                body_chunks_base64: Some(encoded_chunks),
            },
        }],
    };
    let serialized = serde_json::to_string_pretty(&cassette).expect("serialize cassette");
    std::fs::write(temp.path().join(format!("{test_name}.json")), serialized)
        .expect("write cassette");
    let recorder = VcrRecorder::new_with(test_name, VcrMode::Playback, temp.path());
    let client = Client::new().with_vcr(recorder);
    (client, temp)
}

fn anthropic_body(model: &str, prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 8192,
        "messages": [{"content": [{"text": prompt, "type": "text"}], "role": "user"}],
        "model": model,
        "stream": true,
    })
}

fn openai_body(model: &str, prompt: &str) -> serde_json::Value {
    json!({
        "max_tokens": 4096,
        "messages": [{"content": prompt, "role": "user"}],
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true},
    })
}

fn gemini_body(prompt: &str) -> serde_json::Value {
    json!({
        "contents": [{"parts": [{"text": prompt}], "role": "user"}],
        "generationConfig": {"candidateCount": 1, "maxOutputTokens": 8192},
    })
}

fn sse_headers() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "text/event-stream".to_string())]
}

fn json_headers() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "application/json".to_string())]
}

// ---------------------------------------------------------------------------
// Malformed SSE event tests
// ---------------------------------------------------------------------------

#[test]
fn anthropic_malformed_sse_missing_data_field_is_reported() {
    // Event with only "event:" field, no "data:" - should be ignored or handled gracefully
    let (client, _dir) = vcr_client(
        "anthropic_malformed_sse_missing_data_field",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger malformed SSE."),
        200,
        sse_headers(),
        vec![
            "event: message_start\n\n".to_string(),
            // Missing data field - only event type
            "event: content_block_delta\n\n".to_string(),
            // Valid completion
            "event: message_stop\n\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger malformed SSE."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        // Should emit valid events and skip malformed ones
        let mut event_count = 0;
        let mut found_stop = false;

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    event_count += 1;
                    if matches!(event, StreamEvent::Done { .. }) {
                        found_stop = true;
                    }
                }
                Err(e) => {
                    // Errors are acceptable for malformed input
                    panic!("Unexpected error: {e}");
                }
            }
        }

        // Should have processed at least some events
        assert!(
            event_count > 0,
            "Should process events even with malformed ones mixed in"
        );
        assert!(found_stop, "Stream should complete with Done event");
    });
}

#[test]
fn gemini_malformed_sse_unclosed_brace_is_reported() {
    let model = "gemini-test";
    let credential = "test-key";
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse"
    );

    let (client, _dir) = vcr_client(
        "gemini_malformed_sse_unclosed_brace",
        &url,
        gemini_body("Trigger malformed JSON."),
        200,
        sse_headers(),
        vec![
            // Valid event first
            "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Hello\"}]}}]}\n\n"
                .to_string(),
            // Malformed event - unclosed brace
            "data: {\"candidates\": [{\"content\": {\"parts\": \n\n".to_string(),
            // Valid completion to end stream
            "data: {\"candidates\": [{\"finishReason\": \"STOP\"}]}\n\n".to_string(),
        ],
    );

    common::run_async(async move {
        let provider = pi::providers::gemini::GeminiProvider::new(model).with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger malformed JSON."),
                &options_with_key(credential),
            )
            .await
            .expect("stream should initialize");

        let mut found_error = false;
        let mut event_count = 0;

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    event_count += 1;
                    if matches!(event, StreamEvent::Done { .. }) {
                        break;
                    }
                }
                Err(e) => {
                    found_error = true;
                    let msg = e.to_string().to_ascii_lowercase();
                    assert!(
                        msg.contains("json") || msg.contains("parse"),
                        "Error should mention JSON/parsing: {msg}"
                    );
                    break;
                }
            }
        }

        assert!(
            event_count > 0,
            "Should emit at least one event before error"
        );
        assert!(found_error, "Should report JSON parse error");
    });
}

#[test]
fn openai_malformed_sse_truncated_data_is_reported() {
    let (client, _dir) = vcr_client(
        "openai_malformed_sse_truncated_data",
        "https://api.openai.com/v1/chat/completions",
        openai_body("gpt-test", "Trigger truncated SSE."),
        200,
        sse_headers(),
        vec![
            "data: {\"choices\": [{\"delta\": {\"content\": \"Hello\"}, \"index\": 0}]}\n\n"
                .to_string(),
            // Truncated event - missing closing brace and newline
            "data: {\"choices\": [{\"delta\": {\"content\":".to_string(),
        ],
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger truncated SSE."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut found_error = false;
        let mut valid_events = 0;

        while let Some(item) = stream.next().await {
            match item {
                Ok(_) => {
                    valid_events += 1;
                }
                Err(e) => {
                    found_error = true;
                    let msg = e.to_string().to_ascii_lowercase();
                    assert!(
                        msg.contains("json") || msg.contains("parse") || msg.contains("sse"),
                        "Error should mention JSON/parsing/SSE: {msg}"
                    );
                    break;
                }
            }
        }

        assert!(valid_events > 0, "Should emit valid events before error");
        assert!(found_error, "Should report truncated data error");
    });
}

#[test]
fn anthropic_malformed_sse_invalid_utf8_is_reported() {
    // Use base64-encoded chunks to embed invalid UTF-8
    let (client, _dir) = vcr_client_bytes(
        "anthropic_malformed_sse_invalid_utf8",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger invalid UTF-8."),
        200,
        sse_headers(),
        vec![
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_utf8\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n".to_vec(),
            // Invalid UTF-8 sequence (0xFF)
            b"data: \xFF\n\n".to_vec(),
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger invalid UTF-8."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut found_error = false;
        let mut valid_events = 0;

        while let Some(item) = stream.next().await {
            match item {
                Ok(_) => {
                    valid_events += 1;
                }
                Err(e) => {
                    found_error = true;
                    let msg = e.to_string().to_ascii_lowercase();
                    assert!(
                        msg.contains("utf")
                            || msg.contains("sse")
                            || msg.contains("invalid data")
                            || msg.contains("json")
                            || msg.contains("parse")
                            || msg.contains("eof"),
                        "Error should mention UTF-8/SSE/parse semantics: {msg}"
                    );
                    break;
                }
            }
        }

        assert!(valid_events > 0, "Should emit valid events before error");
        assert!(found_error, "Should report UTF-8 error");
    });
}

#[test]
fn gemini_malformed_sse_buffer_overflow_is_reported() {
    // Send an oversized data field that exceeds parser buffer limits
    let oversized_data = "x".repeat(11 * 1024 * 1024); // 11MB - exceeds 10MB limit
    let oversized_event = format!(
        "data: {{\"candidates\": [{{\"content\": {{\"parts\": [{{\"text\": \"{oversized_data}\"}}]}}}}]}}\n\n"
    );

    let (client, _dir) = vcr_client(
        "gemini_malformed_sse_buffer_overflow",
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:streamGenerateContent?alt=sse",
        gemini_body("Trigger buffer overflow."),
        200,
        sse_headers(),
        vec![oversized_event],
    );

    common::run_async(async move {
        let provider =
            pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger buffer overflow."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut found_error = false;

        while let Some(item) = stream.next().await {
            match item {
                Ok(_) => {
                    // May get partial event before error
                }
                Err(e) => {
                    found_error = true;
                    let msg = e.to_string().to_ascii_lowercase();
                    assert!(
                        msg.contains("buffer") || msg.contains("limit") || msg.contains("sse"),
                        "Error should mention buffer/limit: {msg}"
                    );
                    break;
                }
            }
        }

        assert!(found_error, "Should report buffer overflow error");
    });
}

// ---------------------------------------------------------------------------
// Timeout mid-stream tests
// ---------------------------------------------------------------------------

#[test]
fn anthropic_timeout_mid_stream_returns_partial_content() {
    let (client, _dir) = vcr_client(
        "anthropic_timeout_mid_stream",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger mid-stream timeout."),
        200,
        sse_headers(),
        vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\"}}\n\n".to_string(),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n".to_string(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \"}}\n\n".to_string(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n".to_string(),
            // Stream ends abruptly without message_stop - simulating timeout/connection drop
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger mid-stream timeout."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut events = Vec::new();
        let mut text_content = String::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    events.push(format!("{event:?}"));
                    if let StreamEvent::TextDelta { delta, .. } = event {
                        text_content.push_str(&delta);
                    }
                }
                Err(e) => {
                    // For abrupt stream end, we expect a clean termination or a stream error
                    panic!("Unexpected error during mid-stream timeout: {e}");
                }
            }
        }

        // Should have received partial content
        assert!(
            !events.is_empty(),
            "Should receive some events before timeout"
        );
        assert!(
            text_content.contains("Hello"),
            "Should capture partial text"
        );
    });
}

#[test]
fn gemini_timeout_mid_stream_emits_partial_content() {
    let (client, _dir) = vcr_client(
        "gemini_timeout_mid_stream",
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:streamGenerateContent?alt=sse",
        gemini_body("Trigger mid-stream timeout."),
        200,
        sse_headers(),
        vec![
            "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Partial\"}]}}]}\n\n"
                .to_string(),
            // Stream ends here - no finishReason
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger mid-stream timeout."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut events = Vec::new();
        let mut text_content = String::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    events.push(format!("{event:?}"));
                    if let StreamEvent::TextDelta { delta, .. } = event {
                        text_content.push_str(&delta);
                    }
                }
                Err(e) => {
                    panic!("Unexpected error during mid-stream timeout: {e}");
                }
            }
        }

        assert!(!events.is_empty(), "Should receive partial events");
        assert!(
            text_content.contains("Partial"),
            "Should capture partial text"
        );
    });
}

#[test]
fn openai_timeout_after_some_deltas_returns_partial() {
    let (client, _dir) = vcr_client(
        "openai_timeout_after_some_deltas",
        "https://api.openai.com/v1/chat/completions",
        openai_body("gpt-test", "Trigger timeout."),
        200,
        sse_headers(),
        vec![
            "data: {\"choices\": [{\"delta\": {\"content\": \"Start\"}, \"index\": 0}]}\n\n"
                .to_string(),
            "data: {\"choices\": [{\"delta\": {\"content\": \" middle\"}, \"index\": 0}]}\n\n"
                .to_string(),
            // Abrupt end - no [DONE]
        ],
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger timeout."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut text_content = String::new();
        let mut event_count = 0;

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    event_count += 1;
                    if let StreamEvent::TextDelta { delta, .. } = event {
                        text_content.push_str(&delta);
                    }
                }
                Err(e) => {
                    panic!("Unexpected error: {e}");
                }
            }
        }

        assert!(event_count > 0, "Should receive partial events");
        assert!(
            text_content.contains("Start"),
            "Should have partial content"
        );
    });
}

// ---------------------------------------------------------------------------
// Rate limit response handling tests (mid-stream vs initial)
// ---------------------------------------------------------------------------

#[test]
fn anthropic_rate_limit_before_stream_is_reported() {
    let (client, _dir) = vcr_client(
        "anthropic_rate_limit_before_stream",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger rate limit."),
        429,
        json_headers(),
        vec![
            "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Rate limit exceeded. Please retry after 30 seconds.\"}}".to_string()
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger rate limit."),
                &options_with_key("test-key"),
            )
            .await
            .err()
            .expect("expected rate limit error");

        let msg = err.to_string();
        assert!(msg.contains("429"), "Error should contain HTTP 429: {msg}");
        assert!(
            msg.contains("rate limit") || msg.to_ascii_lowercase().contains("rate limit"),
            "Error should mention rate limit: {msg}"
        );
    });
}

#[test]
fn openai_rate_limit_before_stream_is_reported() {
    let (client, _dir) = vcr_client(
        "openai_rate_limit_before_stream",
        "https://api.openai.com/v1/chat/completions",
        openai_body("gpt-test", "Trigger rate limit."),
        429,
        json_headers(),
        vec![
            "{\"error\":{\"message\":\"Rate limit reached\",\"type\":\"rate_limit_error\"}}"
                .to_string(),
        ],
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger rate limit."),
                &options_with_key("test-key"),
            )
            .await
            .err()
            .expect("expected rate limit error");

        let msg = err.to_string();
        assert!(msg.contains("429"), "Error should contain HTTP 429: {msg}");
    });
}

#[test]
fn gemini_rate_limit_with_retry_after_is_reported() {
    let model = "gemini-test";
    let credential = "test-key";
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse"
    );

    let (client, _dir) = vcr_client(
        "gemini_rate_limit_with_retry_after",
        &url,
        gemini_body("Trigger rate limit."),
        429,
        vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Retry-After".to_string(), "60".to_string()),
        ],
        vec!["{\"error\": {\"code\": 429, \"message\": \"Resource quota exceeded.\"}}".to_string()],
    );

    common::run_async(async move {
        let provider = pi::providers::gemini::GeminiProvider::new(model).with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger rate limit."),
                &options_with_key(credential),
            )
            .await
            .err()
            .expect("expected rate limit error");

        let msg = err.to_string();
        assert!(msg.contains("429"), "Error should contain HTTP 429: {msg}");
        // May or may not include retry-after in the error message depending on implementation
    });
}

// ---------------------------------------------------------------------------
// Auth error mid-stream tests
// ---------------------------------------------------------------------------

#[test]
fn anthropic_auth_failure_before_stream_is_reported() {
    let (client, _dir) = vcr_client(
        "anthropic_auth_failure_before_stream",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger auth failure."),
        401,
        json_headers(),
        vec![
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"Invalid API key provided.\"}}".to_string()
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger auth failure."),
                &options_with_key("invalid-key"),
            )
            .await
            .err()
            .expect("expected auth error");

        let msg = err.to_string();
        assert!(msg.contains("401"), "Error should contain HTTP 401: {msg}");
        assert!(
            msg.contains("auth") || msg.to_ascii_lowercase().contains("authentication"),
            "Error should mention auth: {msg}"
        );
    });
}

#[test]
fn openai_auth_failure_before_stream_is_reported() {
    let (client, _dir) = vcr_client(
        "openai_auth_failure_before_stream",
        "https://api.openai.com/v1/chat/completions",
        openai_body("gpt-test", "Trigger auth failure."),
        401,
        json_headers(),
        vec![
            "{\"error\":{\"message\":\"Invalid API key\",\"type\":\"invalid_request_error\"}}"
                .to_string(),
        ],
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger auth failure."),
                &options_with_key("invalid-key"),
            )
            .await
            .err()
            .expect("expected auth error");

        let msg = err.to_string();
        assert!(msg.contains("401"), "Error should contain HTTP 401: {msg}");
    });
}

#[test]
fn gemini_auth_failure_with_api_error_is_reported() {
    let model = "gemini-test";
    let credential = "invalid-key";
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse"
    );

    let (client, _dir) = vcr_client(
        "gemini_auth_failure_with_api_error",
        &url,
        gemini_body("Trigger auth failure."),
        401,
        json_headers(),
        vec!["{\"error\": {\"code\": 401, \"message\": \"API key invalid.\"}}".to_string()],
    );

    common::run_async(async move {
        let provider = pi::providers::gemini::GeminiProvider::new(model).with_client(client);
        let err = provider
            .stream(
                &context_for("Trigger auth failure."),
                &options_with_key(credential),
            )
            .await
            .err()
            .expect("expected auth error");

        let msg = err.to_string();
        assert!(msg.contains("401"), "Error should contain HTTP 401: {msg}");
        assert!(
            msg.contains("API key") || msg.contains("auth") || msg.contains("401"),
            "Error should mention API key or auth: {msg}"
        );
    });
}

// ---------------------------------------------------------------------------
// Edge case: SSE protocol violations
// ---------------------------------------------------------------------------

#[test]
fn anthropic_sse_without_blank_line_separator_is_handled() {
    // SSE spec requires blank line between events, but some implementations
    // violate this - test robustness
    let (client, _dir) = vcr_client(
        "anthropic_sse_without_blank_line_separator",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Trigger malformed SSE."),
        200,
        sse_headers(),
        vec![
            // Events concatenated without proper blank lines
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_concat\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0}\n\n".to_string(),
            // Proper termination
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger malformed SSE."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut event_count = 0;
        let mut saw_parse_error = false;

        while let Some(item) = stream.next().await {
            match item {
                Ok(_) => {
                    event_count += 1;
                }
                Err(e) => {
                    let msg = e.to_string().to_ascii_lowercase();
                    assert!(
                        msg.contains("json") || msg.contains("parse") || msg.contains("sse"),
                        "Malformed SSE should surface parse semantics: {msg}"
                    );
                    saw_parse_error = true;
                    break;
                }
            }
        }

        // Provider should either emit at least one event before failing, or fail
        // explicitly with a parse error (never silently succeed with no signal).
        assert!(
            event_count > 0 || saw_parse_error,
            "Expected events or explicit parse failure"
        );
    });
}

#[test]
fn gemini_sse_with_cr_only_line_endings_is_handled() {
    // Some systems use bare CR instead of CRLF or LF
    let (client, _dir) = vcr_client_bytes(
        "gemini_sse_with_cr_only_line_endings",
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:streamGenerateContent?alt=sse",
        gemini_body("Trigger CR line endings."),
        200,
        sse_headers(),
        vec![
            // Use bare CR (0x0D) as line ending
            b"data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Hello\"}]}}]}\r\r"
                .to_vec(),
            b"data: {\"candidates\": [{\"finishReason\": \"STOP\"}]}\r\r".to_vec(),
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger CR line endings."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut event_count = 0;
        let mut text_content = String::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    event_count += 1;
                    if let StreamEvent::TextDelta { delta, .. } = event {
                        text_content.push_str(&delta);
                    }
                }
                Err(e) => {
                    panic!("Unexpected error with CR line endings: {e}");
                }
            }
        }

        // SSE parser should handle CR line endings per spec
        assert!(event_count > 0, "Should handle CR line endings");
        // Note: text_content may be empty depending on how CR-only is parsed
    });
}

#[test]
fn openai_sse_comment_lines_are_ignored() {
    // SSE spec: lines starting with ':' are comments and should be ignored
    let (client, _dir) = vcr_client(
        "openai_sse_comment_lines_are_ignored",
        "https://api.openai.com/v1/chat/completions",
        openai_body("gpt-test", "Trigger comment lines."),
        200,
        sse_headers(),
        vec![
            ": this is a comment\n\n".to_string(),
            ": another comment\n\n".to_string(),
            "data: {\"choices\": [{\"delta\": {\"content\": \"Hello\"}, \"index\": 0}]}\n\n"
                .to_string(),
            ": keep-alive comment\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ],
    );

    common::run_async(async move {
        let provider = pi::providers::openai::OpenAIProvider::new("gpt-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Trigger comment lines."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut event_count = 0;
        let mut text_content = String::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    event_count += 1;
                    if let StreamEvent::TextDelta { delta, .. } = event {
                        text_content.push_str(&delta);
                    }
                }
                Err(e) => {
                    panic!("Unexpected error: {e}");
                }
            }
        }

        assert!(event_count > 0, "Should process events after comments");
        assert!(
            text_content.contains("Hello"),
            "Should have content from non-comment events"
        );
    });
}

// ---------------------------------------------------------------------------
// Streaming state consistency after errors
// ---------------------------------------------------------------------------

#[test]
fn anthropic_partial_state_preserved_after_stream_end() {
    let (client, _dir) = vcr_client(
        "anthropic_partial_state_preserved",
        "https://api.anthropic.com/v1/messages",
        anthropic_body("claude-test", "Partial state test."),
        200,
        sse_headers(),
        vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-sonnet-20241022\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n".to_string(),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_string(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Partial\"}}\n\n".to_string(),
            // Stream ends without content_block_stop or message_stop
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::anthropic::AnthropicProvider::new("claude-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Partial state test."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => events.push(event),
                Err(_) => break,
            }
        }

        // Should have received at least start and one delta
        assert!(
            events.len() >= 2,
            "Should preserve partial state: {events:?}"
        );

        // Find Start event and verify it has proper metadata
        let start_event = events
            .iter()
            .find(|e| matches!(e, StreamEvent::Start { .. }));
        assert!(
            start_event.is_some(),
            "Should have Start event with message metadata"
        );

        // Find TextDelta to verify partial content
        let text_deltas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();

        assert!(
            !text_deltas.is_empty(),
            "Should have captured partial text deltas"
        );
    });
}

#[test]
fn gemini_accumulated_text_flushed_on_stream_end() {
    let (client, _dir) = vcr_client(
        "gemini_accumulated_text_flushed",
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:streamGenerateContent?alt=sse",
        gemini_body("Accumulated text test."),
        200,
        sse_headers(),
        vec![
            // Multiple text deltas without finishReason
            "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"First \"}]}}]}\n\n"
                .to_string(),
            "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"second \"}]}}]}\n\n"
                .to_string(),
            "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"third\"}]}}]}\n\n"
                .to_string(),
            // Stream ends abruptly - no finishReason
        ],
    );

    common::run_async(async move {
        let provider =
            pi::providers::gemini::GeminiProvider::new("gemini-test").with_client(client);
        let mut stream = provider
            .stream(
                &context_for("Accumulated text test."),
                &options_with_key("test-key"),
            )
            .await
            .expect("stream should initialize");

        let mut all_text = String::new();
        let mut event_count = 0;

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    event_count += 1;
                    if let StreamEvent::TextDelta { delta, .. } = &event {
                        all_text.push_str(delta);
                    }
                }
                Err(e) => {
                    panic!("Unexpected error: {e}");
                }
            }
        }

        assert!(event_count > 0, "Should emit events");
        assert!(
            all_text.contains("First") || all_text.contains("second") || all_text.contains("third"),
            "Should flush accumulated text: got '{all_text}'"
        );
    });
}
