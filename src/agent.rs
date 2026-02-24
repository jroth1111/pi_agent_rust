//! Agent runtime - the core orchestration loop.
//!
//! The agent coordinates between:
//! - Provider: Makes LLM API calls
//! - Tools: Executes tool calls from the assistant
//! - Session: Persists conversation history
//!
//! The main loop:
//! 1. Receive user input
//! 2. Build context (system prompt + history + tools)
//! 3. Stream completion from provider
//! 4. If tool calls: execute tools, append results, goto 3
//! 5. If done: return final message

use crate::auth::AuthStorage;
use crate::compaction::{self, ResolvedCompactionSettings};
use crate::compaction_worker::{CompactionQuota, CompactionWorkerState};
use crate::config::{Config, ReliabilityEnforcementMode, RetrySettings};
use crate::error::{Error, Result};
use crate::events::Action;
use crate::extension_events::{InputEventOutcome, apply_input_event_response};
use crate::extension_tools::collect_extension_tool_wrappers;
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExecMediationLedgerEntry, ExtensionDeliverAs, ExtensionEventName,
    ExtensionHostActions, ExtensionLoadSpec, ExtensionManager, ExtensionPolicy, ExtensionRegion,
    ExtensionRuntimeHandle, ExtensionSendMessage, ExtensionSendUserMessage, JsExtensionLoadSpec,
    JsExtensionRuntimeHandle, NativeRustExtensionLoadSpec, NativeRustExtensionRuntimeHandle,
    RepairPolicyMode, SECURITY_ALERT_SCHEMA_VERSION, SecurityAlert, SecurityAlertAction,
    SecurityAlertCategory, SecurityAlertSeverity, resolve_extension_load_spec,
    sha256_hex_standalone,
};
#[cfg(feature = "wasm-host")]
use crate::extensions::{WasmExtensionHost, WasmExtensionLoadSpec};
use crate::extensions_js::{PiJsRuntimeConfig, RepairMode};
use crate::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, CustomMessage, ImageContent, Message,
    StopReason, StreamEvent, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage,
    UserContent, UserMessage,
};
use crate::models::{ModelEntry, ModelRegistry};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
#[allow(unused_imports)]
use crate::reliability::{
    ArtifactStore, Attempt, AttemptStats, CloseOutcomeKind, ClosePayload, CloseResult,
    EvidenceRecord, FsArtifactStore, LeaseManager, RetryAgent, RetryConfig, ReviewSubmission,
    ReviewerScorer, StateDigest, StuckDetector, TokenUsage, VerificationOutcome,
};
use crate::session::{AutosaveFlushTrigger, Session, SessionHandle};
use crate::state::{
    CompletionGate as StateCompletionGate, DiscoveryPriority, DiscoveryTracker, Evidence,
    EvidenceGate, TaskEventKind, TaskEventLog, TaskResult,
};
use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
use crate::verification::{EvidenceCollector, TestQualityGate};
use asupersync::sync::{Mutex, Notify};
use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream;
use serde::Serialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const MAX_CONCURRENT_TOOLS: usize = 8;
const RELIABILITY_OBJECTIVE_MAX_CHARS: usize = 160;

// ============================================================================
// Agent Configuration
// ============================================================================

/// Configuration for the agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// System prompt to use for all requests.
    pub system_prompt: Option<String>,

    /// Maximum tool call iterations before stopping.
    pub max_tool_iterations: usize,

    /// Default stream options.
    pub stream_options: StreamOptions,

    /// Strip image blocks before sending context to providers.
    pub block_images: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_tool_iterations: 50,
            stream_options: StreamOptions::default(),
            block_images: false,
        }
    }
}

/// Async fetcher for queued messages (steering or follow-up).
pub type MessageFetcher = Arc<dyn Fn() -> BoxFuture<'static, Vec<Message>> + Send + Sync + 'static>;

type AgentEventHandler = Arc<dyn Fn(AgentEvent) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

impl QueueMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum QueueKind {
    Steering,
    FollowUp,
}

#[derive(Debug, Clone)]
struct QueuedMessage {
    seq: u64,
    enqueued_at: i64,
    message: Message,
}

#[derive(Debug)]
struct MessageQueue {
    steering: VecDeque<QueuedMessage>,
    follow_up: VecDeque<QueuedMessage>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    next_seq: u64,
}

impl MessageQueue {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
            next_seq: 0,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    fn pending_count(&self) -> usize {
        self.steering.len() + self.follow_up.len()
    }

    fn push(&mut self, kind: QueueKind, message: Message) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let entry = QueuedMessage {
            seq,
            enqueued_at: Utc::now().timestamp_millis(),
            message,
        };
        match kind {
            QueueKind::Steering => self.steering.push_back(entry),
            QueueKind::FollowUp => self.follow_up.push_back(entry),
        }
        seq
    }

    fn push_steering(&mut self, message: Message) -> u64 {
        self.push(QueueKind::Steering, message)
    }

    fn push_follow_up(&mut self, message: Message) -> u64 {
        self.push(QueueKind::FollowUp, message)
    }

    fn pop_steering(&mut self) -> Vec<Message> {
        self.pop_kind(QueueKind::Steering)
    }

    fn pop_follow_up(&mut self) -> Vec<Message> {
        self.pop_kind(QueueKind::FollowUp)
    }

    fn pop_kind(&mut self, kind: QueueKind) -> Vec<Message> {
        let (queue, mode) = match kind {
            QueueKind::Steering => (&mut self.steering, self.steering_mode),
            QueueKind::FollowUp => (&mut self.follow_up, self.follow_up_mode),
        };

        match mode {
            QueueMode::All => queue.drain(..).map(|entry| entry.message).collect(),
            QueueMode::OneAtATime => queue
                .pop_front()
                .into_iter()
                .map(|entry| entry.message)
                .collect(),
        }
    }
}

// ============================================================================
// Agent Event
// ============================================================================

/// Events emitted by the agent during execution.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Agent lifecycle start.
    AgentStart {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Agent lifecycle end with all new messages.
    AgentEnd {
        #[serde(rename = "sessionId")]
        session_id: String,
        messages: Vec<Message>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Turn lifecycle start (assistant response + tool calls).
    TurnStart {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "turnIndex")]
        turn_index: usize,
        timestamp: i64,
    },
    /// Turn lifecycle end with tool results.
    TurnEnd {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "turnIndex")]
        turn_index: usize,
        message: Message,
        #[serde(rename = "toolResults")]
        tool_results: Vec<Message>,
    },
    /// Message lifecycle start (user, assistant, or tool result).
    MessageStart { message: Message },
    /// Message update (assistant streaming).
    MessageUpdate {
        message: Message,
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: Box<AssistantMessageEvent>,
    },
    /// Message lifecycle end.
    MessageEnd { message: Message },
    /// Tool execution start.
    ToolExecutionStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        args: serde_json::Value,
    },
    /// Tool execution update.
    ToolExecutionUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        args: serde_json::Value,
        #[serde(rename = "partialResult")]
        partial_result: ToolOutput,
    },
    /// Tool execution end.
    ToolExecutionEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        result: ToolOutput,
        #[serde(rename = "isError")]
        is_error: bool,
    },
    /// Auto-compaction lifecycle start.
    AutoCompactionStart { reason: String },
    /// Auto-compaction lifecycle end.
    AutoCompactionEnd {
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        aborted: bool,
        #[serde(rename = "willRetry")]
        will_retry: bool,
        #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    /// Auto-retry lifecycle start.
    AutoRetryStart {
        attempt: u32,
        #[serde(rename = "maxAttempts")]
        max_attempts: u32,
        #[serde(rename = "delayMs")]
        delay_ms: u64,
        #[serde(rename = "errorMessage")]
        error_message: String,
    },
    /// Auto-retry lifecycle end.
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        #[serde(rename = "finalError", skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    /// Extension error during event dispatch or execution.
    ExtensionError {
        #[serde(rename = "extensionId", skip_serializing_if = "Option::is_none")]
        extension_id: Option<String>,
        event: String,
        error: String,
    },
}

// ============================================================================
// Agent
// ============================================================================

/// Handle to request an abort of an in-flight agent run.
#[derive(Debug, Clone)]
pub struct AbortHandle {
    inner: Arc<AbortSignalInner>,
}

/// Signal for observing abort requests.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    inner: Arc<AbortSignalInner>,
}

#[derive(Debug)]
struct AbortSignalInner {
    aborted: AtomicBool,
    notify: Notify,
}

impl AbortHandle {
    /// Create a new abort handle + signal pair.
    #[must_use]
    pub fn new() -> (Self, AbortSignal) {
        let inner = Arc::new(AbortSignalInner {
            aborted: AtomicBool::new(false),
            notify: Notify::new(),
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            AbortSignal { inner },
        )
    }

    /// Trigger an abort.
    pub fn abort(&self) {
        if !self.inner.aborted.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }
}

impl AbortSignal {
    /// Check if an abort has already been requested.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        if self.is_aborted() {
            return;
        }

        loop {
            self.inner.notify.notified().await;
            if self.is_aborted() {
                return;
            }
        }
    }
}

/// The agent runtime that orchestrates LLM calls and tool execution.
pub struct Agent {
    /// The LLM provider.
    provider: Arc<dyn Provider>,

    /// Tool registry.
    tools: ToolRegistry,

    /// Agent configuration.
    config: AgentConfig,

    /// Optional extension manager for tool/event hooks.
    extensions: Option<ExtensionManager>,

    /// Message history.
    messages: Vec<Message>,

    /// Fetchers for queued steering messages (interrupts).
    steering_fetchers: Vec<MessageFetcher>,

    /// Fetchers for queued follow-up messages (idle).
    follow_up_fetchers: Vec<MessageFetcher>,

    /// Internal queue for steering/follow-up messages.
    message_queue: MessageQueue,

    /// Cached tool definitions. Invalidated when tools change via `extend_tools`.
    cached_tool_defs: Option<Vec<ToolDef>>,

    /// Optional per-turn scope objective used by the scope boundary guard.
    scope_objective: Option<String>,
}

impl Agent {
    /// Create a new agent with the given provider and tools.
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
            extensions: None,
            messages: Vec::new(),
            steering_fetchers: Vec::new(),
            follow_up_fetchers: Vec::new(),
            message_queue: MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime),
            cached_tool_defs: None,
            scope_objective: None,
        }
    }

    /// Get the current message history.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Clear the message history.
    pub fn clear_messages(&mut self) {
        self.messages.clear();
    }

    /// Add a message to the history.
    pub fn add_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Replace the message history.
    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Set the scope objective for the current execution turn.
    pub fn set_scope_objective(&mut self, objective: Option<String>) {
        self.scope_objective = objective
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
    }

    /// Clear any active scope objective.
    pub fn clear_scope_objective(&mut self) {
        self.scope_objective = None;
    }

    /// Replace the provider implementation (used for model/provider switching).
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
    }

    /// Register async fetchers for queued steering/follow-up messages.
    ///
    /// This is additive: multiple sources (e.g. RPC, extensions) can register
    /// fetchers, and the agent will poll all of them.
    pub fn register_message_fetchers(
        &mut self,
        steering: Option<MessageFetcher>,
        follow_up: Option<MessageFetcher>,
    ) {
        if let Some(fetcher) = steering {
            self.steering_fetchers.push(fetcher);
        }
        if let Some(fetcher) = follow_up {
            self.follow_up_fetchers.push(fetcher);
        }
    }

    /// Extend the tool registry with additional tools (e.g. extension-registered tools).
    pub fn extend_tools<I>(&mut self, tools: I)
    where
        I: IntoIterator<Item = Box<dyn Tool>>,
    {
        self.tools.extend(tools);
        self.cached_tool_defs = None; // Invalidate cache when tools change
    }

    /// Queue a steering message (delivered after tool completion).
    pub fn queue_steering(&mut self, message: Message) -> u64 {
        self.message_queue.push_steering(message)
    }

    /// Queue a follow-up message (delivered when agent becomes idle).
    pub fn queue_follow_up(&mut self, message: Message) -> u64 {
        self.message_queue.push_follow_up(message)
    }

    /// Configure queue delivery modes.
    pub const fn set_queue_modes(&mut self, steering: QueueMode, follow_up: QueueMode) {
        self.message_queue.set_modes(steering, follow_up);
    }

    /// Count queued messages (steering + follow-up).
    #[must_use]
    pub fn queued_message_count(&self) -> usize {
        self.message_queue.pending_count()
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    pub const fn stream_options(&self) -> &StreamOptions {
        &self.config.stream_options
    }

    pub const fn stream_options_mut(&mut self) -> &mut StreamOptions {
        &mut self.config.stream_options
    }

    /// Build tool definitions for the API, caching results across turns.
    fn build_tool_defs(&mut self) -> Vec<ToolDef> {
        if let Some(cached) = &self.cached_tool_defs {
            return cached.clone();
        }
        let defs: Vec<ToolDef> = self
            .tools
            .tools()
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        self.cached_tool_defs = Some(defs.clone());
        defs
    }

    /// Build context for a completion request.
    fn build_context(&mut self) -> Context<'_> {
        let messages: Cow<'_, [Message]> = if self.config.block_images {
            let mut msgs = self.messages.clone();
            // Filter out hidden custom messages.
            msgs.retain(|m| match m {
                Message::Custom(c) => c.display,
                _ => true,
            });
            let stats = filter_images_for_provider(&mut msgs);
            if stats.removed_images > 0 {
                tracing::debug!(
                    filtered_images = stats.removed_images,
                    affected_messages = stats.affected_messages,
                    "Filtered image content from outbound provider context (images.block_images=true)"
                );
            }
            Cow::Owned(msgs)
        } else {
            // Check if we need to filter hidden custom messages to avoid cloning if not needed.
            let has_hidden = self.messages.iter().any(|m| match m {
                Message::Custom(c) => !c.display,
                _ => false,
            });

            if has_hidden {
                let mut msgs = self.messages.clone();
                msgs.retain(|m| match m {
                    Message::Custom(c) => c.display,
                    _ => true,
                });
                Cow::Owned(msgs)
            } else {
                Cow::Borrowed(self.messages.as_slice())
            }
        };

        // Borrow cached tool defs if available; otherwise build + cache + borrow.
        if self.cached_tool_defs.is_none() {
            let defs: Vec<ToolDef> = self
                .tools
                .tools()
                .iter()
                .map(|t| ToolDef {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: t.parameters(),
                })
                .collect();
            self.cached_tool_defs = Some(defs);
        }
        let tools = Cow::Borrowed(self.cached_tool_defs.as_deref().unwrap());

        Context {
            system_prompt: self.config.system_prompt.as_deref().map(Cow::Borrowed),
            messages,
            tools,
        }
    }

    /// Run the agent with a user message.
    ///
    /// Returns a stream of events and the final assistant message.
    pub async fn run(
        &mut self,
        user_input: impl Into<String>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_abort(user_input, None, on_event).await
    }

    /// Run the agent with a user message and abort support.
    pub async fn run_with_abort(
        &mut self,
        user_input: impl Into<String>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        // Add user message
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(user_input.into()),
            timestamp: Utc::now().timestamp_millis(),
        });

        // Run the agent loop
        self.run_loop(vec![user_message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with structured content (text + images).
    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_content_with_abort(content, None, on_event)
            .await
    }

    /// Run the agent with structured content (text + images) and abort support.
    pub async fn run_with_content_with_abort(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        // Add user message
        let user_message = Message::User(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: Utc::now().timestamp_millis(),
        });

        // Run the agent loop
        self.run_loop(vec![user_message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with a pre-constructed user message and abort support.
    pub async fn run_with_message_with_abort(
        &mut self,
        message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(vec![message], Arc::new(on_event), abort)
            .await
    }

    /// Continue the agent loop without adding a new prompt message (used for retries).
    pub async fn run_continue_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(Vec::new(), Arc::new(on_event), abort).await
    }

    fn build_abort_message(&self, partial: Option<AssistantMessage>) -> AssistantMessage {
        let mut message = partial.unwrap_or_else(|| AssistantMessage {
            content: Vec::new(),
            api: self.provider.api().to_string(),
            provider: self.provider.name().to_string(),
            model: self.provider.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Aborted,
            error_message: Some("Aborted".to_string()),
            timestamp: Utc::now().timestamp_millis(),
        });
        message.stop_reason = StopReason::Aborted;
        message.error_message = Some("Aborted".to_string());
        message.timestamp = Utc::now().timestamp_millis();
        message
    }

    /// The main agent loop.
    #[allow(clippy::too_many_lines)]
    async fn run_loop(
        &mut self,
        prompts: Vec<Message>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        let session_id = self
            .config
            .stream_options
            .session_id
            .clone()
            .unwrap_or_default();
        let mut iterations = 0usize;
        let mut turn_index: usize = 0;
        let mut new_messages: Vec<Message> = Vec::with_capacity(prompts.len() + 8);
        let mut last_assistant: Option<AssistantMessage> = None;

        let agent_start_event = AgentEvent::AgentStart {
            session_id: session_id.clone(),
        };
        self.dispatch_extension_lifecycle_event(&agent_start_event)
            .await;
        on_event(agent_start_event);

        for prompt in prompts {
            on_event(AgentEvent::MessageStart {
                message: prompt.clone(),
            });
            self.messages.push(prompt.clone());
            let end_msg = prompt.clone();
            new_messages.push(prompt); // move, no clone
            on_event(AgentEvent::MessageEnd { message: end_msg });
        }

        // Delivery boundary: start of turn (steering messages queued while idle).
        let mut pending_messages = self.drain_steering_messages().await;

        loop {
            let mut has_more_tool_calls = true;
            let mut steering_after_tools: Option<Vec<Message>> = None;

            while has_more_tool_calls || !pending_messages.is_empty() {
                let current_turn_index = turn_index;
                let turn_start_event = AgentEvent::TurnStart {
                    session_id: session_id.clone(),
                    turn_index: current_turn_index,
                    timestamp: Utc::now().timestamp_millis(),
                };
                self.dispatch_extension_lifecycle_event(&turn_start_event)
                    .await;
                on_event(turn_start_event);

                for message in std::mem::take(&mut pending_messages) {
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    self.messages.push(message.clone());
                    let end_msg = message.clone();
                    new_messages.push(message); // move, no clone
                    on_event(AgentEvent::MessageEnd { message: end_msg });
                }

                if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                    let abort_message = self.build_abort_message(last_assistant.clone());
                    let message = Message::assistant(abort_message.clone());
                    if !matches!(self.messages.last(), Some(Message::Assistant(_))) {
                        self.messages.push(message.clone());
                        new_messages.push(message.clone());
                        on_event(AgentEvent::MessageStart {
                            message: message.clone(),
                        });
                    }
                    on_event(AgentEvent::MessageEnd {
                        message: message.clone(),
                    });
                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message,
                        tool_results: Vec::new(),
                    };
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    on_event(turn_end_event);
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: std::mem::take(&mut new_messages),
                        error: Some(
                            abort_message
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "Aborted".to_string()),
                        ),
                    };
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    on_event(agent_end_event);
                    return Ok(abort_message);
                }

                let assistant_message = match self
                    .stream_assistant_response(Arc::clone(&on_event), abort.clone())
                    .await
                {
                    Ok(msg) => msg,
                    Err(err) => {
                        let agent_end_event = AgentEvent::AgentEnd {
                            session_id: session_id.clone(),
                            messages: std::mem::take(&mut new_messages),
                            error: Some(err.to_string()),
                        };
                        self.dispatch_extension_lifecycle_event(&agent_end_event)
                            .await;
                        on_event(agent_end_event);
                        return Err(err);
                    }
                };
                // Wrap in Arc once; share via Arc::clone (O(1)) instead of deep
                // cloning the full AssistantMessage for every consumer.
                let assistant_arc = Arc::new(assistant_message);
                last_assistant = Some((*assistant_arc).clone());

                let assistant_event_message = Message::Assistant(Arc::clone(&assistant_arc));
                new_messages.push(assistant_event_message.clone());

                if matches!(
                    assistant_arc.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message: assistant_event_message.clone(),
                        tool_results: Vec::new(),
                    };
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    on_event(turn_end_event);
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: std::mem::take(&mut new_messages),
                        error: assistant_arc.error_message.clone(),
                    };
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    on_event(agent_end_event);
                    return Ok(Arc::unwrap_or_clone(assistant_arc));
                }

                let tool_calls = extract_tool_calls(&assistant_arc.content);
                has_more_tool_calls = !tool_calls.is_empty();

                let mut tool_results: Vec<Arc<ToolResultMessage>> = Vec::new();
                if has_more_tool_calls {
                    iterations += 1;
                    if iterations > self.config.max_tool_iterations {
                        let error_message = format!(
                            "Maximum tool iterations ({}) exceeded",
                            self.config.max_tool_iterations
                        );
                        let mut stop_message = (*assistant_arc).clone();
                        stop_message.stop_reason = StopReason::Error;
                        stop_message.error_message = Some(error_message.clone());
                        let stop_arc = Arc::new(stop_message.clone());
                        let stop_event_message = Message::Assistant(Arc::clone(&stop_arc));

                        // Keep in-memory transcript and event payloads aligned with the
                        // error stop result returned to callers.
                        if let Some(last @ Message::Assistant(_)) = self.messages.last_mut() {
                            *last = stop_event_message.clone();
                        }
                        if let Some(last @ Message::Assistant(_)) = new_messages.last_mut() {
                            *last = stop_event_message.clone();
                        }

                        let turn_end_event = AgentEvent::TurnEnd {
                            session_id: session_id.clone(),
                            turn_index: current_turn_index,
                            message: stop_event_message,
                            tool_results: Vec::new(),
                        };
                        self.dispatch_extension_lifecycle_event(&turn_end_event)
                            .await;
                        on_event(turn_end_event);

                        let agent_end_event = AgentEvent::AgentEnd {
                            session_id: session_id.clone(),
                            messages: std::mem::take(&mut new_messages),
                            error: Some(error_message),
                        };
                        self.dispatch_extension_lifecycle_event(&agent_end_event)
                            .await;
                        on_event(agent_end_event);

                        return Ok(stop_message);
                    }

                    let outcome = match self
                        .execute_tool_calls(
                            &tool_calls,
                            Arc::clone(&on_event),
                            &mut new_messages,
                            abort.clone(),
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            let agent_end_event = AgentEvent::AgentEnd {
                                session_id: session_id.clone(),
                                messages: std::mem::take(&mut new_messages),
                                error: Some(err.to_string()),
                            };
                            self.dispatch_extension_lifecycle_event(&agent_end_event)
                                .await;
                            on_event(agent_end_event);
                            return Err(err);
                        }
                    };
                    tool_results = outcome.tool_results;
                    steering_after_tools = outcome.steering_messages;
                }

                let tool_messages = tool_results
                    .iter()
                    .map(|r| Message::ToolResult(Arc::clone(r)))
                    .collect::<Vec<_>>();

                let turn_end_event = AgentEvent::TurnEnd {
                    session_id: session_id.clone(),
                    turn_index: current_turn_index,
                    message: assistant_event_message.clone(),
                    tool_results: tool_messages,
                };
                self.dispatch_extension_lifecycle_event(&turn_end_event)
                    .await;
                on_event(turn_end_event);

                turn_index = turn_index.saturating_add(1);

                if let Some(steering) = steering_after_tools.take() {
                    pending_messages = steering;
                } else {
                    // Delivery boundary: after assistant completion (no tool calls).
                    pending_messages = self.drain_steering_messages().await;
                }
            }

            // Delivery boundary: agent idle (after all tool calls + steering).
            let follow_up = self.drain_follow_up_messages().await;
            if follow_up.is_empty() {
                break;
            }
            pending_messages = follow_up;
        }

        let Some(final_message) = last_assistant else {
            return Err(Error::api("Agent completed without assistant message"));
        };

        let agent_end_event = AgentEvent::AgentEnd {
            session_id: session_id.clone(),
            messages: new_messages,
            error: None,
        };
        self.dispatch_extension_lifecycle_event(&agent_end_event)
            .await;
        on_event(agent_end_event);
        Ok(final_message)
    }

    async fn fetch_messages(&self, fetcher: Option<&MessageFetcher>) -> Vec<Message> {
        if let Some(fetcher) = fetcher {
            (fetcher)().await
        } else {
            Vec::new()
        }
    }

    async fn dispatch_extension_lifecycle_event(&self, event: &AgentEvent) {
        let Some(extensions) = &self.extensions else {
            return;
        };

        let name = match event {
            AgentEvent::AgentStart { .. } => ExtensionEventName::AgentStart,
            AgentEvent::AgentEnd { .. } => ExtensionEventName::AgentEnd,
            AgentEvent::TurnStart { .. } => ExtensionEventName::TurnStart,
            AgentEvent::TurnEnd { .. } => ExtensionEventName::TurnEnd,
            _ => return,
        };

        let payload = match serde_json::to_value(event) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!("failed to serialize agent lifecycle event (fail-open): {err}");
                return;
            }
        };

        if let Err(err) = extensions.dispatch_event(name, Some(payload)).await {
            tracing::warn!("agent lifecycle extension hook failed (fail-open): {err}");
        }
    }

    async fn drain_steering_messages(&mut self) -> Vec<Message> {
        for fetcher in &self.steering_fetchers {
            let fetched = self.fetch_messages(Some(fetcher)).await;
            for message in fetched {
                self.message_queue.push_steering(message);
            }
        }
        self.message_queue.pop_steering()
    }

    async fn drain_follow_up_messages(&mut self) -> Vec<Message> {
        for fetcher in &self.follow_up_fetchers {
            let fetched = self.fetch_messages(Some(fetcher)).await;
            for message in fetched {
                self.message_queue.push_follow_up(message);
            }
        }
        self.message_queue.pop_follow_up()
    }

    /// Stream an assistant response and emit message events.
    #[allow(clippy::too_many_lines)]
    async fn stream_assistant_response(
        &mut self,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        // Build context and stream completion
        let provider = Arc::clone(&self.provider);
        let stream_options = self.config.stream_options.clone();
        let context = self.build_context();
        let mut stream = provider.stream(&context, &stream_options).await?;

        let mut added_partial = false;
        // Track whether we've already emitted `MessageStart` for this streaming response.
        // Avoids cloning the full message on every event just to re-emit a redundant start.
        let mut sent_start = false;

        loop {
            let event_result = if let Some(signal) = abort.as_ref() {
                let abort_fut = signal.wait().fuse();
                let event_fut = stream.next().fuse();
                futures::pin_mut!(abort_fut, event_fut);

                match futures::future::select(abort_fut, event_fut).await {
                    futures::future::Either::Left(((), _event_fut)) => {
                        let last_partial = if added_partial {
                            match self.messages.last() {
                                Some(Message::Assistant(a)) => Some((**a).clone()),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        let abort_arc = Arc::new(self.build_abort_message(last_partial));
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&abort_arc)),
                            assistant_message_event: Box::new(AssistantMessageEvent::Error {
                                reason: StopReason::Aborted,
                                error: Arc::clone(&abort_arc),
                            }),
                        });
                        return Ok(self.finalize_assistant_message(
                            Arc::try_unwrap(abort_arc).unwrap_or_else(|a| (*a).clone()),
                            &on_event,
                            added_partial,
                        ));
                    }
                    futures::future::Either::Right((event, _abort_fut)) => event,
                }
            } else {
                stream.next().await
            };

            let Some(event_result) = event_result else {
                break;
            };
            let event = event_result?;

            match event {
                StreamEvent::Start { partial } => {
                    let shared = Arc::new(partial);
                    self.update_partial_message(Arc::clone(&shared), &mut added_partial);
                    on_event(AgentEvent::MessageStart {
                        message: Message::Assistant(Arc::clone(&shared)),
                    });
                    sent_start = true;
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(Arc::clone(&shared)),
                        assistant_message_event: Box::new(AssistantMessageEvent::Start {
                            partial: shared,
                        }),
                    });
                }
                StreamEvent::TextStart { content_index, .. } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::Text(TextContent::new("")));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(AssistantMessageEvent::TextStart {
                                content_index,
                                partial: shared,
                            }),
                        });
                    }
                }
                StreamEvent::TextDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if let Some(ContentBlock::Text(text)) =
                                msg.content.get_mut(content_index)
                            {
                                text.text.push_str(&delta);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(AssistantMessageEvent::TextDelta {
                                content_index,
                                delta,
                                partial: shared,
                            }),
                        });
                    }
                }
                StreamEvent::TextEnd {
                    content_index,
                    content,
                    ..
                } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if let Some(ContentBlock::Text(text)) =
                                msg.content.get_mut(content_index)
                            {
                                text.text.clone_from(&content);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(AssistantMessageEvent::TextEnd {
                                content_index,
                                content,
                                partial: shared,
                            }),
                        });
                    }
                }
                StreamEvent::ThinkingStart { content_index, .. } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                            }));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(
                                AssistantMessageEvent::ThinkingStart {
                                    content_index,
                                    partial: shared,
                                },
                            ),
                        });
                    }
                }
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if let Some(ContentBlock::Thinking(thinking)) =
                                msg.content.get_mut(content_index)
                            {
                                thinking.thinking.push_str(&delta);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(
                                AssistantMessageEvent::ThinkingDelta {
                                    content_index,
                                    delta,
                                    partial: shared,
                                },
                            ),
                        });
                    }
                }
                StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                    ..
                } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if let Some(ContentBlock::Thinking(thinking)) =
                                msg.content.get_mut(content_index)
                            {
                                thinking.thinking.clone_from(&content);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(AssistantMessageEvent::ThinkingEnd {
                                content_index,
                                content,
                                partial: shared,
                            }),
                        });
                    }
                }
                StreamEvent::ToolCallStart { content_index, .. } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::ToolCall(ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: serde_json::Value::Null,
                                thought_signature: None,
                            }));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(
                                AssistantMessageEvent::ToolCallStart {
                                    content_index,
                                    partial: shared,
                                },
                            ),
                        });
                    }
                }
                StreamEvent::ToolCallDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        // No mutation needed for ToolCallDelta – args stay Null until ToolCallEnd.
                        // Just share the current Arc (O(1) refcount bump, zero deep copies).
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(
                                AssistantMessageEvent::ToolCallDelta {
                                    content_index,
                                    delta,
                                    partial: shared,
                                },
                            ),
                        });
                    }
                }
                StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    ..
                } => {
                    if let Some(Message::Assistant(msg_arc)) = self.messages.last_mut() {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if let Some(ContentBlock::ToolCall(tc)) =
                                msg.content.get_mut(content_index)
                            {
                                *tc = tool_call.clone();
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: Box::new(AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call,
                                partial: shared,
                            }),
                        });
                    }
                }
                StreamEvent::Done { message, .. } => {
                    return Ok(self.finalize_assistant_message(message, &on_event, added_partial));
                }
                StreamEvent::Error { error, .. } => {
                    return Ok(self.finalize_assistant_message(error, &on_event, added_partial));
                }
            }
        }

        // If the stream ends without a Done/Error event, we may have a partial message.
        // Instead of discarding it, we finalize it with an error state so the user/session
        // retains the partial content.
        if added_partial {
            if let Some(Message::Assistant(last_msg)) = self.messages.last() {
                let mut final_msg = (**last_msg).clone();
                final_msg.stop_reason = StopReason::Error;
                final_msg.error_message = Some("Stream ended without Done event".to_string());
                return Ok(self.finalize_assistant_message(final_msg, &on_event, true));
            }
        }
        Err(Error::api("Stream ended without Done event"))
    }

    /// Update the partial assistant message in `self.messages`.
    ///
    /// Takes an `Arc<AssistantMessage>` and moves it into the message list
    /// (one Arc move, zero deep-copies).
    fn update_partial_message(
        &mut self,
        partial: Arc<AssistantMessage>,
        added_partial: &mut bool,
    ) -> bool {
        if *added_partial {
            if let Some(last @ Message::Assistant(_)) = self.messages.last_mut() {
                *last = Message::Assistant(partial);
            } else {
                // Defensive: added_partial is true but last message isn't Assistant.
                // Push as new message rather than silently dropping the update.
                tracing::warn!("update_partial_message: expected last message to be Assistant");
                self.messages.push(Message::Assistant(partial));
            }
            false
        } else {
            self.messages.push(Message::Assistant(partial));
            *added_partial = true;
            true
        }
    }

    fn finalize_assistant_message(
        &mut self,
        message: AssistantMessage,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        added_partial: bool,
    ) -> AssistantMessage {
        let arc = Arc::new(message);
        if added_partial {
            if let Some(last @ Message::Assistant(_)) = self.messages.last_mut() {
                *last = Message::Assistant(Arc::clone(&arc));
            } else {
                // Defensive: added_partial is true but last message isn't Assistant.
                // Push as new message rather than overwriting an unrelated message.
                tracing::warn!("finalize_assistant_message: expected last message to be Assistant");
                self.messages.push(Message::Assistant(Arc::clone(&arc)));
                on_event(AgentEvent::MessageStart {
                    message: Message::Assistant(Arc::clone(&arc)),
                });
            }
        } else {
            self.messages.push(Message::Assistant(Arc::clone(&arc)));
            on_event(AgentEvent::MessageStart {
                message: Message::Assistant(Arc::clone(&arc)),
            });
        }

        on_event(AgentEvent::MessageEnd {
            message: Message::Assistant(Arc::clone(&arc)),
        });
        Arc::try_unwrap(arc).unwrap_or_else(|a| (*a).clone())
    }

    async fn execute_parallel_batch(
        &self,
        batch: Vec<(usize, ToolCall)>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Vec<(usize, (ToolOutput, bool))> {
        let futures = batch.into_iter().map(|(idx, tc)| {
            let on_event = Arc::clone(&on_event);
            async move { (idx, self.execute_tool_owned(tc, on_event).await) }
        });

        if let Some(signal) = abort.as_ref() {
            use futures::future::{Either, select};
            let all_fut = stream::iter(futures)
                .buffer_unordered(MAX_CONCURRENT_TOOLS)
                .collect::<Vec<_>>()
                .fuse();
            let abort_fut = signal.wait().fuse();
            futures::pin_mut!(all_fut, abort_fut);

            match select(all_fut, abort_fut).await {
                Either::Left((batch_results, _)) => batch_results,
                Either::Right(_) => Vec::new(), // Aborted
            }
        } else {
            stream::iter(futures)
                .buffer_unordered(MAX_CONCURRENT_TOOLS)
                .collect::<Vec<_>>()
                .await
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        on_event: AgentEventHandler,
        new_messages: &mut Vec<Message>,
        abort: Option<AbortSignal>,
    ) -> Result<ToolExecutionOutcome> {
        let mut results = Vec::new();
        let mut steering_messages: Option<Vec<Message>> = None;

        if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
            return Ok(ToolExecutionOutcome {
                tool_results: results,
                steering_messages,
            });
        }

        // Phase 1: Emit start events for ALL tools up front.
        for tool_call in tool_calls {
            on_event(AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            });
        }

        // Phase 2: Execute tools with safety barriers.
        let mut pending_parallel: Vec<(usize, ToolCall)> = Vec::new();
        let mut tool_outputs: Vec<Option<(ToolOutput, bool)>> = vec![None; tool_calls.len()];

        // Iterate through tools. If read-only, buffer. If unsafe, flush buffer then run unsafe.
        for (index, tool_call) in tool_calls.iter().enumerate() {
            if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                break;
            }

            if self.should_block_tool_call_for_scope(tool_call) {
                tool_outputs[index] = Some((self.scope_blocked_tool_output(tool_call), true));
                continue;
            }

            let is_read_only =
                matches!(self.tools.get(&tool_call.name), Some(tool) if tool.is_read_only());

            if is_read_only {
                pending_parallel.push((index, tool_call.clone()));
            } else {
                // Check steering BEFORE flushing parallel or running unsafe.
                let steering = self.drain_steering_messages().await;
                if !steering.is_empty() {
                    steering_messages = Some(steering);
                    break;
                }

                // Barrier: flush parallel buffer first
                if !pending_parallel.is_empty() {
                    let batch = std::mem::take(&mut pending_parallel);
                    let results = self
                        .execute_parallel_batch(batch, Arc::clone(&on_event), abort.clone())
                        .await;
                    for (idx, result) in results {
                        tool_outputs[idx] = Some(result);
                    }
                }

                if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                    break;
                }

                // Execute unsafe tool sequentially
                // Check steering AGAIN before the potentially expensive unsafe tool
                let steering = self.drain_steering_messages().await;
                if !steering.is_empty() {
                    steering_messages = Some(steering);
                    break;
                }

                let result = self
                    .execute_tool(tool_call.clone(), Arc::clone(&on_event))
                    .await;
                tool_outputs[index] = Some(result);
            }
        }

        // Flush remaining parallel tools
        if !pending_parallel.is_empty()
            && !abort.as_ref().is_some_and(AbortSignal::is_aborted)
            && steering_messages.is_none()
        {
            let batch = std::mem::take(&mut pending_parallel);
            // Check steering one last time before final flush
            let steering = self.drain_steering_messages().await;
            if steering.is_empty() {
                let results = self
                    .execute_parallel_batch(batch, Arc::clone(&on_event), abort.clone())
                    .await;
                for (idx, result) in results {
                    tool_outputs[idx] = Some(result);
                }
            } else {
                steering_messages = Some(steering);
            }
        }

        // Phase 3: Process results sequentially and handle skips.
        for (index, tool_call) in tool_calls.iter().enumerate() {
            // Check for new steering if we haven't already found some.
            // This catches steering messages that arrived during the *last* tool's execution.
            if steering_messages.is_none() && !abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                let steering = self.drain_steering_messages().await;
                if !steering.is_empty() {
                    steering_messages = Some(steering);
                }
            }

            // Extract the result, tracking whether the tool actually executed.
            // If `tool_outputs[index]` is `Some`, `execute_tool` ran.
            // If `None`, the tool was skipped/aborted.
            if let Some((output, is_error)) = tool_outputs[index].take() {
                // Tool executed normally.
                // Always emit ToolExecutionEnd to close the lifecycle.
                on_event(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    result: ToolOutput {
                        content: output.content.clone(),
                        details: output.details.clone(),
                        is_error,
                    },
                    is_error,
                });

                let tool_result = Arc::new(ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: output.content,
                    details: output.details,
                    is_error,
                    timestamp: Utc::now().timestamp_millis(),
                });

                let msg = Message::ToolResult(Arc::clone(&tool_result));
                self.messages.push(msg.clone());
                on_event(AgentEvent::MessageStart {
                    message: msg.clone(),
                });
                let end_msg = msg.clone();
                new_messages.push(msg);
                on_event(AgentEvent::MessageEnd { message: end_msg });

                results.push(tool_result);
            } else if steering_messages.is_some() {
                // Skipped due to steering.
                results.push(self.skip_tool_call(tool_call, &on_event, new_messages));
            } else {
                // Aborted or otherwise failed to run (e.g. abort signal).
                let output = ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(
                        "Tool execution aborted",
                    ))],
                    details: None,
                    is_error: true,
                };

                on_event(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    args: tool_call.arguments.clone(),
                    partial_result: ToolOutput {
                        content: output.content.clone(),
                        details: output.details.clone(),
                        is_error: true,
                    },
                });

                on_event(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    result: ToolOutput {
                        content: output.content.clone(),
                        details: output.details.clone(),
                        is_error: true,
                    },
                    is_error: true,
                });

                let tool_result = Arc::new(ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: output.content,
                    details: output.details,
                    is_error: true,
                    timestamp: Utc::now().timestamp_millis(),
                });

                let msg = Message::ToolResult(Arc::clone(&tool_result));
                self.messages.push(msg.clone());
                on_event(AgentEvent::MessageStart {
                    message: msg.clone(),
                });
                let end_msg = msg.clone();
                new_messages.push(msg);
                on_event(AgentEvent::MessageEnd { message: end_msg });

                results.push(tool_result);
            }
        }

        Ok(ToolExecutionOutcome {
            tool_results: results,
            steering_messages,
        })
    }

    fn should_block_tool_call_for_scope(&self, tool_call: &ToolCall) -> bool {
        let Some(objective) = self.scope_objective.as_deref() else {
            return false;
        };

        // Only hard-block when the user objective carries explicit scope anchors
        // (paths/modules). Broad natural-language objectives should not be gated.
        if !Self::objective_has_explicit_scope_anchor(objective) {
            return false;
        }

        if !ReliabilityTraceCapture::is_mutating_action(&tool_call.name, &tool_call.arguments) {
            return false;
        }

        let Some(target) =
            ReliabilityTraceCapture::scope_target(&tool_call.name, &tool_call.arguments)
        else {
            return false;
        };

        !ReliabilityTraceCapture::target_matches_objective(objective, &target)
    }

    fn objective_has_explicit_scope_anchor(objective: &str) -> bool {
        objective.split_whitespace().any(|token| {
            let trimmed = token.trim_matches(|c: char| {
                matches!(
                    c,
                    ',' | '.' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'
                )
            });
            let known_extension = std::path::Path::new(trimmed)
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("rs")
                        || ext.eq_ignore_ascii_case("toml")
                        || ext.eq_ignore_ascii_case("md")
                });
            trimmed.contains('/') || known_extension
        })
    }

    fn scope_blocked_tool_output(&self, tool_call: &ToolCall) -> ToolOutput {
        let target = ReliabilityTraceCapture::scope_target(&tool_call.name, &tool_call.arguments)
            .unwrap_or_else(|| "requested target".to_string());
        let objective = self
            .scope_objective
            .as_deref()
            .unwrap_or("current objective");
        let message = format!(
            "Scope guard blocked out-of-scope '{}' action for target '{}'; objective='{}'. Logged as discovery for follow-up.",
            tool_call.name, target, objective
        );

        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message))],
            details: Some(json!({
                "scopeBlocked": true,
                "toolName": tool_call.name,
                "target": target,
                "objective": objective,
            })),
            is_error: true,
        }
    }

    async fn execute_tool(
        &self,
        tool_call: ToolCall,
        on_event: AgentEventHandler,
    ) -> (ToolOutput, bool) {
        let extensions = self.extensions.clone();

        let (mut output, is_error) = if let Some(extensions) = &extensions {
            match Self::dispatch_tool_call_hook(extensions, &tool_call).await {
                Some(blocked_output) => (blocked_output, true),
                None => {
                    self.execute_tool_without_hooks(&tool_call, Arc::clone(&on_event))
                        .await
                }
            }
        } else {
            self.execute_tool_without_hooks(&tool_call, Arc::clone(&on_event))
                .await
        };

        if let Some(extensions) = &extensions {
            Self::record_builtin_bash_mediation(extensions, &tool_call, &output);
            Self::apply_tool_result_hook(extensions, &tool_call, &mut output, is_error).await;
        }

        (output, is_error)
    }

    async fn execute_tool_owned(
        &self,
        tool_call: ToolCall,
        on_event: AgentEventHandler,
    ) -> (ToolOutput, bool) {
        self.execute_tool(tool_call, on_event).await
    }

    async fn execute_tool_without_hooks(
        &self,
        tool_call: &ToolCall,
        on_event: AgentEventHandler,
    ) -> (ToolOutput, bool) {
        // Find the tool
        let Some(tool) = self.tools.get(&tool_call.name) else {
            return (Self::tool_not_found_output(&tool_call.name), true);
        };

        let tool_name = tool_call.name.clone();
        let tool_id = tool_call.id.clone();
        let tool_args = tool_call.arguments.clone();
        let on_event = Arc::clone(&on_event);

        let update_callback = move |update: ToolUpdate| {
            on_event(AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                args: tool_args.clone(),
                partial_result: ToolOutput {
                    content: update.content,
                    details: update.details,
                    is_error: false,
                },
            });
        };

        match tool
            .execute(
                &tool_call.id,
                tool_call.arguments.clone(),
                Some(Box::new(update_callback)),
            )
            .await
        {
            Ok(output) => {
                let is_error = output.is_error;
                (output, is_error)
            }
            Err(e) => (
                ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!("Error: {e}")))],
                    details: None,
                    is_error: true,
                },
                true,
            ),
        }
    }

    fn tool_not_found_output(tool_name: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Error: Tool '{tool_name}' not found"
            )))],
            details: None,
            is_error: true,
        }
    }

    async fn dispatch_tool_call_hook(
        extensions: &ExtensionManager,
        tool_call: &ToolCall,
    ) -> Option<ToolOutput> {
        match extensions
            .dispatch_tool_call(tool_call, EXTENSION_EVENT_TIMEOUT_MS)
            .await
        {
            Ok(Some(result)) if result.block => {
                Some(Self::tool_call_blocked_output(result.reason.as_deref()))
            }
            Ok(_) => None,
            Err(err) => {
                tracing::warn!("tool_call extension hook failed (fail-open): {err}");
                None
            }
        }
    }

    fn tool_call_blocked_output(reason: Option<&str>) -> ToolOutput {
        let reason = reason.map(str::trim).filter(|reason| !reason.is_empty());
        let message = reason.map_or_else(
            || "Tool execution was blocked by an extension".to_string(),
            |reason| format!("Tool execution blocked: {reason}"),
        );

        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message))],
            details: None,
            is_error: true,
        }
    }

    async fn apply_tool_result_hook(
        extensions: &ExtensionManager,
        tool_call: &ToolCall,
        output: &mut ToolOutput,
        is_error: bool,
    ) {
        match extensions
            .dispatch_tool_result(tool_call, &*output, is_error, EXTENSION_EVENT_TIMEOUT_MS)
            .await
        {
            Ok(Some(result)) => {
                if let Some(content) = result.content {
                    output.content = content;
                }
                if let Some(details) = result.details {
                    output.details = Some(details);
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("tool_result extension hook failed (fail-open): {err}"),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn record_builtin_bash_mediation(
        extensions: &ExtensionManager,
        tool_call: &ToolCall,
        output: &ToolOutput,
    ) {
        if tool_call.name != "bash" {
            return;
        }
        let Some(details) = output.details.as_ref() else {
            return;
        };
        let Some(exec_mediation) = details.get("execMediation").and_then(Value::as_object) else {
            return;
        };

        let decision = exec_mediation
            .get("decision")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|decision| !decision.is_empty())
            .unwrap_or("allow");
        let command_hash = exec_mediation
            .get("commandHash")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|hash| !hash.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                tool_call
                    .arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .map(sha256_hex_standalone)
            })
            .unwrap_or_default();
        if command_hash.is_empty() {
            return;
        }

        let command_class = exec_mediation
            .get("commandClass")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let risk_tier = exec_mediation
            .get("riskTier")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let reason = exec_mediation
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let policy_source = exec_mediation
            .get("policySource")
            .and_then(Value::as_str)
            .unwrap_or("builtin_bash_exec_mediation")
            .to_string();
        let ts_ms = Utc::now().timestamp_millis();

        extensions.record_exec_mediation(ExecMediationLedgerEntry {
            ts_ms,
            extension_id: Some("builtin:bash".to_string()),
            command_hash: command_hash.clone(),
            command_class: command_class.clone(),
            risk_tier,
            decision: decision.to_string(),
            reason: reason.clone(),
        });

        let (severity, action, summary) = match decision {
            "deny" => (
                SecurityAlertSeverity::Error,
                SecurityAlertAction::Deny,
                format!(
                    "Builtin bash denied by exec mediation: {}",
                    if reason.is_empty() {
                        "policy decision"
                    } else {
                        reason.as_str()
                    }
                ),
            ),
            "allow_with_audit" => (
                SecurityAlertSeverity::Info,
                SecurityAlertAction::Harden,
                format!(
                    "Builtin bash allowed with audit: {}",
                    if reason.is_empty() {
                        "classified command"
                    } else {
                        reason.as_str()
                    }
                ),
            ),
            _ => return,
        };
        let mut reason_codes = Vec::new();
        if let Some(class) = command_class {
            reason_codes.push(class);
        } else if !reason.is_empty() {
            reason_codes.push(reason);
        }

        extensions.record_security_alert(SecurityAlert {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms,
            sequence_id: 0,
            extension_id: "builtin:bash".to_string(),
            category: SecurityAlertCategory::ExecMediation,
            severity,
            capability: "exec".to_string(),
            method: "bash".to_string(),
            reason_codes,
            summary,
            policy_source,
            action,
            remediation: if decision == "deny" {
                "Review the command and adjust exec mediation policy if intended.".to_string()
            } else {
                String::new()
            },
            risk_score: 0.0,
            risk_state: None,
            context_hash: command_hash,
        });
    }

    fn skip_tool_call(
        &mut self,
        tool_call: &ToolCall,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        new_messages: &mut Vec<Message>,
    ) -> Arc<ToolResultMessage> {
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "Skipped due to queued user message.",
            ))],
            details: None,
            is_error: true,
        };

        // Note: Phase 1 already emitted ToolExecutionStart for all tools,
        // so we only emit Update and End here.
        on_event(AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            partial_result: output.clone(),
        });
        on_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result: output.clone(),
            is_error: true,
        });

        let tool_result = Arc::new(ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: output.content,
            details: output.details,
            is_error: true,
            timestamp: Utc::now().timestamp_millis(),
        });

        let msg = Message::ToolResult(Arc::clone(&tool_result));
        self.messages.push(msg.clone());
        new_messages.push(msg.clone());

        on_event(AgentEvent::MessageStart {
            message: msg.clone(),
        });
        on_event(AgentEvent::MessageEnd { message: msg });

        tool_result
    }
}

// ============================================================================
// Agent Session (Agent + Session persistence)
// ============================================================================

struct ToolExecutionOutcome {
    tool_results: Vec<Arc<ToolResultMessage>>,
    steering_messages: Option<Vec<Message>>,
}

#[derive(Clone)]
struct RetryExecutionSnapshot {
    session: Session,
    agent_messages: Vec<Message>,
}

struct RetryExecutionOutcome {
    attempt_id: String,
    snapshot: RetryExecutionSnapshot,
    result: Result<AssistantMessage>,
}

#[derive(Clone)]
enum RetryInput {
    Text(String),
    Content(Vec<ContentBlock>),
}

/// Pre-created extension runtime state for overlapping startup I/O.
///
/// By spawning runtime boot as a background task *before* session creation and
/// model selection, expensive runtime startup can overlap with other work.
pub struct PreWarmedExtensionRuntime {
    /// The extension manager (already has `cwd` and risk config set).
    pub manager: ExtensionManager,
    /// The booted runtime handle.
    pub runtime: ExtensionRuntimeHandle,
    /// The tool registry passed to the runtime during boot.
    pub tools: Arc<ToolRegistry>,
}

pub struct AgentSession {
    pub agent: Agent,
    pub session: Arc<Mutex<Session>>,
    save_enabled: bool,
    retry_settings: RetrySettings,
    reliability_enabled: bool,
    reliability_enforcement_mode: ReliabilityEnforcementMode,
    /// Extension lifecycle region — ensures the JS runtime thread is shut
    /// down when the session ends.
    pub extensions: Option<ExtensionRegion>,
    extensions_is_streaming: Arc<AtomicBool>,
    compaction_settings: ResolvedCompactionSettings,
    compaction_worker: CompactionWorkerState,
    model_registry: Option<ModelRegistry>,
    auth_storage: Option<AuthStorage>,
}

#[derive(Debug, Default)]
struct ExtensionInjectedQueue {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
}

impl ExtensionInjectedQueue {
    fn push_steering(&mut self, message: Message) {
        self.steering.push_back(message);
    }

    fn push_follow_up(&mut self, message: Message) {
        self.follow_up.push_back(message);
    }

    fn pop_steering(&mut self) -> Vec<Message> {
        self.steering.drain(..).collect()
    }

    fn pop_follow_up(&mut self) -> Vec<Message> {
        self.follow_up.drain(..).collect()
    }
}

#[derive(Clone)]
struct AgentSessionHostActions {
    session: Arc<Mutex<Session>>,
    injected: Arc<StdMutex<ExtensionInjectedQueue>>,
    is_streaming: Arc<AtomicBool>,
}

impl AgentSessionHostActions {
    fn enqueue(&self, deliver_as: Option<ExtensionDeliverAs>, message: Message) {
        let deliver_as = deliver_as.unwrap_or(ExtensionDeliverAs::Steer);
        let Ok(mut queue) = self.injected.lock() else {
            return;
        };
        match deliver_as {
            ExtensionDeliverAs::FollowUp => {
                queue.push_follow_up(message);
            }
            ExtensionDeliverAs::Steer | ExtensionDeliverAs::NextTurn => {
                queue.push_steering(message);
            }
        }
    }

    async fn append_to_session(&self, message: Message) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        session.append_model_message(message);
        Ok(())
    }
}

#[async_trait]
impl ExtensionHostActions for AgentSessionHostActions {
    async fn send_message(&self, message: ExtensionSendMessage) -> Result<()> {
        let custom_message = Message::Custom(CustomMessage {
            content: message.content,
            custom_type: message.custom_type,
            display: message.display,
            details: message.details,
            timestamp: Utc::now().timestamp_millis(),
        });

        if matches!(message.deliver_as, Some(ExtensionDeliverAs::NextTurn)) {
            return self.append_to_session(custom_message).await;
        }

        if self.is_streaming.load(Ordering::SeqCst) {
            self.enqueue(message.deliver_as, custom_message);
            return Ok(());
        }

        // Non-streaming, best-effort: persist to session. Triggering a new turn is handled by the
        // interactive layer; non-interactive modes will pick this up on the next prompt.
        let _ = message.trigger_turn;
        self.append_to_session(custom_message).await
    }

    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()> {
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(message.text),
            timestamp: Utc::now().timestamp_millis(),
        });

        if self.is_streaming.load(Ordering::SeqCst) {
            self.enqueue(message.deliver_as, user_message);
            return Ok(());
        }

        // Non-streaming, best-effort: persist to session. Interactive mode triggers turns via UI.
        self.append_to_session(user_message).await
    }
}

#[derive(Debug, Clone)]
struct ReliabilityTraceSnapshot {
    task_id: String,
    objective: String,
    saw_execute: bool,
    evidence: Vec<EvidenceRecord>,
    phase_violations: Vec<String>,
    stuck_patterns: Vec<crate::reliability::StuckPattern>,
    discoveries: Vec<crate::state::Discovery>,
    discovery_summary: Option<crate::state::DiscoverySummary>,
    task_events: TaskEventLog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReliabilityPhase {
    Discover,
    Plan,
    Execute,
    Verify,
    Close,
}

#[derive(Debug)]
struct ReliabilityTraceCapture {
    snapshot: ReliabilityTraceSnapshot,
    bash_commands_by_call: HashMap<String, String>,
    env_id: Option<String>,
    artifact_store: Option<Arc<FsArtifactStore>>,
    phase: ReliabilityPhase,
    lease_manager: LeaseManager,
    lease_id: String,
    fence_token: u64,
    stuck_detector: StuckDetector,
    discovery_tracker: DiscoveryTracker,
    last_stuck_pattern: Option<String>,
    discovery_fingerprints: std::collections::HashSet<String>,
}

impl ReliabilityTraceCapture {
    fn new(
        task_id: String,
        objective: String,
        env_id: Option<String>,
        artifact_store: Option<Arc<FsArtifactStore>>,
    ) -> Self {
        let mut task_events = TaskEventLog::new();
        task_events.append(TaskEventKind::Created, Some("agent".to_string()));

        let mut lease_manager = LeaseManager::default();
        let (lease_id, fence_token) = match lease_manager.issue_lease(&task_id, "agent", 3600) {
            Ok(grant) => (grant.lease_id, grant.fence_token),
            Err(err) => {
                tracing::warn!("failed to issue reliability lease: {err}");
                (String::new(), 0)
            }
        };
        Self {
            snapshot: ReliabilityTraceSnapshot {
                task_id,
                objective,
                saw_execute: false,
                evidence: Vec::new(),
                phase_violations: Vec::new(),
                stuck_patterns: Vec::new(),
                discoveries: Vec::new(),
                discovery_summary: None,
                task_events,
            },
            bash_commands_by_call: HashMap::new(),
            env_id,
            artifact_store,
            phase: ReliabilityPhase::Discover,
            lease_manager,
            lease_id,
            fence_token,
            stuck_detector: StuckDetector::new(),
            discovery_tracker: DiscoveryTracker::new(),
            last_stuck_pattern: None,
            discovery_fingerprints: std::collections::HashSet::new(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn observe_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let action = Self::action_from_tool(tool_name, args);
                self.stuck_detector.record_action(&action);
                self.stuck_detector.record_tool_round();
                self.maybe_track_discovery(tool_name, args);

                if Self::is_mutating_action(tool_name, args) {
                    self.on_mutating_action(tool_name);
                }
                if tool_name == "bash" {
                    if let Some(command) = Self::maybe_extract_bash_command(args) {
                        self.bash_commands_by_call
                            .insert(tool_call_id.clone(), command);
                    }
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                self.snapshot.saw_execute = true;
                self.snapshot.task_events.append(
                    TaskEventKind::AttemptRecorded {
                        action: tool_name.clone(),
                        success: !*is_error,
                    },
                    Some("agent".to_string()),
                );

                if *is_error {
                    let output = Self::extract_verify_output(result);
                    if output.trim().is_empty() {
                        self.stuck_detector
                            .record_error(&format!("{tool_name} returned an error"));
                    } else {
                        self.stuck_detector.record_error(&output);
                    }
                }

                if tool_name != "bash" {
                    self.check_stuck_pattern();
                    self.refresh_discovery_snapshot();
                    return;
                }
                let Some(command) = self.bash_commands_by_call.remove(tool_call_id) else {
                    self.check_stuck_pattern();
                    self.refresh_discovery_snapshot();
                    return;
                };
                if !Self::is_verify_command(&command) {
                    self.check_stuck_pattern();
                    self.refresh_discovery_snapshot();
                    return;
                }
                if self.phase != ReliabilityPhase::Execute {
                    self.snapshot.phase_violations.push(format!(
                        "verify command executed from invalid phase {:?}",
                        self.phase
                    ));
                }
                self.phase = ReliabilityPhase::Verify;
                if let Err(err) = self.validate_submit_fence(&self.lease_id, self.fence_token) {
                    self.snapshot
                        .phase_violations
                        .push(format!("stale lease/fence submission rejected: {err}"));
                }

                let exit_code = Self::parse_exit_code(result, *is_error);
                let stdout = Self::extract_verify_output(result);
                let mut artifact_ids = Vec::new();
                if !stdout.is_empty() {
                    if let Some(store) = &self.artifact_store {
                        match store.put_text(&self.snapshot.task_id, "stdout", &stdout) {
                            Ok(id) => artifact_ids.push(id),
                            Err(err) => {
                                tracing::warn!(
                                    "failed to persist reliability stdout artifact: {err}"
                                );
                            }
                        }
                    }
                }
                if artifact_ids.is_empty() {
                    let fallback_artifact_id = result
                        .details
                        .as_ref()
                        .and_then(|value| value.get("fullOutputPath"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    if let Some(artifact_id) = fallback_artifact_id {
                        artifact_ids.push(artifact_id);
                    }
                }
                let evidence = EvidenceRecord::from_command_output_with_env(
                    self.snapshot.task_id.clone(),
                    command,
                    exit_code,
                    &stdout,
                    "",
                    artifact_ids,
                    self.env_id.clone(),
                );
                self.snapshot.evidence.push(evidence);
            }
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => {
                if matches!(
                    assistant_message_event.as_ref(),
                    AssistantMessageEvent::TextDelta { .. }
                ) {
                    self.stuck_detector.record_text_round();
                }
            }
            _ => {}
        }

        self.check_stuck_pattern();
        self.refresh_discovery_snapshot();
    }

    fn snapshot(&self) -> ReliabilityTraceSnapshot {
        self.snapshot.clone()
    }

    fn on_mutating_action(&mut self, tool_name: &str) {
        if self.phase == ReliabilityPhase::Discover {
            self.phase = ReliabilityPhase::Plan;
        }
        match self.phase {
            ReliabilityPhase::Plan | ReliabilityPhase::Execute => {
                self.phase = ReliabilityPhase::Execute;
            }
            ReliabilityPhase::Verify | ReliabilityPhase::Close => {
                self.snapshot.phase_violations.push(format!(
                    "mutating action '{tool_name}' executed after verification phase"
                ));
            }
            ReliabilityPhase::Discover => {}
        }
    }

    fn is_mutating_action(tool_name: &str, args: &Value) -> bool {
        if tool_name == "bash" {
            return Self::maybe_extract_bash_command(args)
                .as_deref()
                .is_none_or(|command| !Self::is_verify_command(command));
        }
        !matches!(tool_name, "read" | "grep" | "find" | "ls" | "websearch")
    }

    fn action_from_tool(tool_name: &str, args: &Value) -> Action {
        fn arg<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
            keys.iter()
                .find_map(|key| args.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        }

        match tool_name {
            "read" => Action::read(arg(args, &["file_path", "path"]).unwrap_or("<unknown>")),
            "write" => Action::write(
                arg(args, &["file_path", "path"]).unwrap_or("<unknown>"),
                arg(args, &["content"]).unwrap_or(""),
            ),
            "edit" => Action::edit(
                arg(args, &["file_path", "path"]).unwrap_or("<unknown>"),
                arg(args, &["old_string", "old"]).unwrap_or(""),
                arg(args, &["new_string", "new"]).unwrap_or(""),
            ),
            "bash" => Action::bash(arg(args, &["command"]).unwrap_or("")),
            "grep" => Action::grep(arg(args, &["pattern"]).unwrap_or("")),
            "find" => Action::glob(arg(args, &["pattern", "glob"]).unwrap_or("")),
            "ls" => Action::ls(),
            "websearch" => Action::web_search(arg(args, &["query"]).unwrap_or("")),
            "webfetch" => Action::web_fetch(arg(args, &["url"]).unwrap_or("")),
            _ => Action::Custom {
                name: tool_name.to_string(),
                arguments: args.clone(),
            },
        }
    }

    fn maybe_track_discovery(&mut self, tool_name: &str, args: &Value) {
        if !Self::is_mutating_action(tool_name, args) {
            return;
        }

        let Some(target) = Self::scope_target(tool_name, args) else {
            return;
        };

        if Self::target_matches_objective(&self.snapshot.objective, &target) {
            return;
        }

        let fingerprint = format!("{tool_name}:{target}");
        if self.discovery_fingerprints.contains(&fingerprint) {
            return;
        }
        self.discovery_fingerprints.insert(fingerprint);

        let description = format!(
            "Potential out-of-scope action for objective '{}': {}",
            self.snapshot.objective, target
        );

        if let Ok(discovery) = self.discovery_tracker.discover(
            &description,
            &self.snapshot.task_id,
            DiscoveryPriority::ShouldAddress,
        ) {
            let related_to = self.snapshot.task_events.last().map_or(0, |event| event.id);
            self.snapshot.task_events.append(
                TaskEventKind::DiscoveryMade {
                    description: discovery.description.clone(),
                    related_to,
                    priority: discovery.priority,
                },
                Some("agent".to_string()),
            );
        }
    }

    fn scope_target(tool_name: &str, args: &Value) -> Option<String> {
        let key_candidates: &[&str] = match tool_name {
            "bash" => &["command"],
            "write" | "edit" | "read" => &["file_path", "path"],
            "grep" => &["pattern", "path"],
            "find" => &["pattern", "glob", "path"],
            _ => &["path", "file_path", "query", "url"],
        };

        key_candidates
            .iter()
            .find_map(|key| args.get(*key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    }

    fn target_matches_objective(objective: &str, target: &str) -> bool {
        let objective_tokens = objective
            .to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|token| token.len() >= 4)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if objective_tokens.is_empty() {
            return true;
        }

        let target_lower = target.to_ascii_lowercase();
        objective_tokens
            .iter()
            .any(|token| target_lower.contains(token))
    }

    fn check_stuck_pattern(&mut self) {
        let Some(pattern) = self.stuck_detector.check() else {
            self.last_stuck_pattern = None;
            return;
        };

        let fingerprint = pattern.description();
        if self.last_stuck_pattern.as_deref() == Some(fingerprint.as_str()) {
            return;
        }
        self.last_stuck_pattern = Some(fingerprint.clone());

        self.snapshot.stuck_patterns.push(pattern.clone());
        self.snapshot
            .phase_violations
            .push(format!("stuck pattern detected: {fingerprint}"));
        self.snapshot.task_events.append(
            TaskEventKind::StuckDetected { pattern },
            Some("agent".to_string()),
        );
    }

    fn refresh_discovery_snapshot(&mut self) {
        self.snapshot.discoveries = self.discovery_tracker.all().to_vec();
        self.snapshot.discovery_summary = Some(self.discovery_tracker.summary());
    }

    fn validate_submit_fence(
        &self,
        lease_id: &str,
        fence_token: u64,
    ) -> std::result::Result<(), String> {
        if lease_id.trim().is_empty() {
            return Err("lease id missing".to_string());
        }
        self.lease_manager
            .validate_fence(lease_id, fence_token)
            .map_err(|err| err.to_string())
    }

    fn extract_verify_output(result: &ToolOutput) -> String {
        let full_output_path = result
            .details
            .as_ref()
            .and_then(|value| value.get("fullOutputPath"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if let Some(path) = full_output_path {
            match std::fs::read(&path) {
                Ok(bytes) => return String::from_utf8_lossy(&bytes).into_owned(),
                Err(err) => tracing::warn!("failed reading fullOutputPath '{path}': {err}"),
            }
        }
        Self::flatten_text_content(&result.content)
    }

    fn maybe_extract_bash_command(args: &Value) -> Option<String> {
        args.get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    }

    fn parse_exit_code(result: &ToolOutput, is_error: bool) -> i32 {
        result
            .details
            .as_ref()
            .and_then(|value| value.get("exitCode"))
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or_else(|| i32::from(is_error))
    }

    fn flatten_text_content(content: &[ContentBlock]) -> String {
        let mut out = String::new();
        for block in content {
            if let ContentBlock::Text(text) = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text.text);
            }
        }
        out
    }

    fn is_verify_command(command: &str) -> bool {
        const PREFIXES: &[&str] = &[
            "cargo test",
            "cargo check",
            "cargo clippy",
            "cargo nextest",
            "pytest",
            "npm test",
            "pnpm test",
            "yarn test",
            "go test",
        ];
        let normalized = command.trim().to_ascii_lowercase();
        PREFIXES.iter().any(|prefix| normalized.starts_with(prefix))
    }
}

#[cfg(test)]
mod message_queue_tests {
    use super::*;

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    #[test]
    fn message_queue_one_at_a_time() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("a"));
        queue.push_steering(user_message("b"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            first.first(),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "a")
        ));

        let second = queue.pop_steering();
        assert_eq!(second.len(), 1);
        assert!(matches!(
            second.first(),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "b")
        ));

        assert!(queue.pop_steering().is_empty());
    }

    #[test]
    fn message_queue_all_mode() {
        let mut queue = MessageQueue::new(QueueMode::All, QueueMode::OneAtATime);
        queue.push_steering(user_message("a"));
        queue.push_steering(user_message("b"));

        let drained = queue.pop_steering();
        assert_eq!(drained.len(), 2);
        assert!(queue.pop_steering().is_empty());
    }

    #[test]
    fn message_queue_separates_kinds() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("steer"));
        queue.push_follow_up(user_message("follow"));

        let steering = queue.pop_steering();
        assert_eq!(steering.len(), 1);
        assert_eq!(queue.pending_count(), 1);

        let follow = queue.pop_follow_up();
        assert_eq!(follow.len(), 1);
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn message_queue_seq_increments() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        let first = queue.push_steering(user_message("a"));
        let second = queue.push_follow_up(user_message("b"));
        assert!(second > first);
    }

    #[test]
    fn message_queue_seq_saturates_at_u64_max() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.next_seq = u64::MAX;

        let first = queue.push_steering(user_message("a"));
        let second = queue.push_follow_up(user_message("b"));

        assert_eq!(first, u64::MAX);
        assert_eq!(second, u64::MAX);
        assert_eq!(queue.pending_count(), 2);
    }

    #[test]
    fn message_queue_follow_up_all_mode_drains_entire_queue_in_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::All);
        queue.push_follow_up(user_message("f1"));
        queue.push_follow_up(user_message("f2"));

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 2);
        assert!(matches!(
            follow_up.first(),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "f1")
        ));
        assert!(matches!(
            follow_up.get(1),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "f2")
        ));
        assert!(queue.pop_follow_up().is_empty());
    }
}

#[cfg(test)]
mod extensions_integration_tests {
    use super::*;

    use crate::extensions::{ExecMediationPolicy, SecurityAlertCategory, SecurityAlertSeverity};
    use crate::session::Session;
    use crate::tools::BashTool;
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug)]
    struct NoopProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for NoopProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "count_tool"
        }

        fn label(&self) -> &str {
            "count_tool"
        }

        fn description(&self) -> &str {
            "counting tool"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("ok"))],
                details: None,
                is_error: false,
            })
        }
    }

    #[derive(Debug)]
    struct ToolUseProvider {
        stream_calls: AtomicUsize,
    }

    impl ToolUseProvider {
        const fn new() -> Self {
            Self {
                stream_calls: AtomicUsize::new(0),
            }
        }

        fn assistant_message(
            &self,
            stop_reason: StopReason,
            content: Vec<ContentBlock>,
        ) -> AssistantMessage {
            AssistantMessage {
                content,
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolUseProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);

            let partial = self.assistant_message(StopReason::Stop, Vec::new());

            let (reason, message) = if call_index == 0 {
                let tool_calls = vec![
                    ToolCall {
                        id: "call-1".to_string(),
                        name: "count_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    },
                    ToolCall {
                        id: "call-2".to_string(),
                        name: "count_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    },
                ];

                (
                    StopReason::ToolUse,
                    self.assistant_message(
                        StopReason::ToolUse,
                        tool_calls
                            .into_iter()
                            .map(ContentBlock::ToolCall)
                            .collect::<Vec<_>>(),
                    ),
                )
            } else {
                (
                    StopReason::Stop,
                    self.assistant_message(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new("done"))],
                    ),
                )
            };

            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done { reason, message }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn agent_session_enable_extensions_registers_extension_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "hello_tool",
                    label: "hello_tool",
                    description: "test tool",
                    parameters: { type: "object", properties: { name: { type: "string" } } },
                    execute: async (_callId, input, _onUpdate, _abort, ctx) => {
                      const who = input && input.name ? String(input.name) : "world";
                      const cwd = ctx && ctx.cwd ? String(ctx.cwd) : "";
                      return {
                        content: [{ type: "text", text: `hello ${who}` }],
                        details: { from: "extension", cwd: cwd },
                        isError: false
                      };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool = agent_session
                .agent
                .tools
                .get("hello_tool")
                .expect("hello_tool registered");

            let output = tool
                .execute("call-1", json!({ "name": "pi" }), None)
                .await
                .expect("execute tool");

            assert!(!output.is_error);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected single text content block, got {:?}",
                output.content
            );
            let [ContentBlock::Text(text)] = output.content.as_slice() else {
                return;
            };
            assert_eq!(text.text, "hello pi");

            let details = output.details.expect("details present");
            assert_eq!(
                details.get("from").and_then(serde_json::Value::as_str),
                Some("extension")
            );
        });
    }

    #[test]
    fn agent_session_enable_extensions_rejects_mixed_js_and_native_entries() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let js_entry = temp_dir.path().join("ext.mjs");
            let native_entry = temp_dir.path().join("ext.native.json");
            std::fs::write(
                &js_entry,
                r"
                export default function init(_pi) {}
                ",
            )
            .expect("write js extension entry");
            std::fs::write(&native_entry, "{}").expect("write native extension descriptor");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let err = agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[js_entry, native_entry])
                .await
                .expect_err("mixed extension runtimes should be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("Mixed extension runtimes are not supported"),
                "unexpected mixed-runtime error message: {msg}"
            );
        });
    }

    #[test]
    fn extension_send_message_persists_custom_message_entry_when_idle() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "emit_message",
                    label: "emit_message",
                    description: "emit a custom message",
                    parameters: { type: "object" },
                    execute: async () => {
                      pi.sendMessage({
                        customType: "note",
                        content: "hello",
                        display: true,
                        details: { from: "test" }
                      }, {});
                      return { content: [{ type: "text", text: "ok" }], isError: false };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool = agent_session
                .agent
                .tools
                .get("emit_message")
                .expect("emit_message registered");

            let _ = tool
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session
                .lock(cx.cx())
                .await
                .expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, display, details, .. })
                            if custom_type == "note"
                                && content == "hello"
                                && *display
                                && details.as_ref().and_then(|v| v.get("from").and_then(Value::as_str)) == Some("test")
                    )
                }),
                "expected custom message to be persisted, got {messages:?}"
            );
        });
    }

    #[test]
    fn extension_send_message_persists_custom_message_entry_when_idle_after_await() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "emit_message",
                    label: "emit_message",
                    description: "emit a custom message",
                    parameters: { type: "object" },
                    execute: async () => {
                      await Promise.resolve();
                      pi.sendMessage({
                        customType: "note",
                        content: "hello-after-await",
                        display: true,
                        details: { from: "test" }
                      }, {});
                      return { content: [{ type: "text", text: "ok" }], isError: false };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool = agent_session
                .agent
                .tools
                .get("emit_message")
                .expect("emit_message registered");

            let _ = tool
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session
                .lock(cx.cx())
                .await
                .expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, display, details, .. })
                            if custom_type == "note"
                                && content == "hello-after-await"
                                && *display
                                && details.as_ref().and_then(|v| v.get("from").and_then(Value::as_str)) == Some("test")
                    )
                }),
                "expected custom message to be persisted, got {messages:?}"
            );
        });
    }

    #[test]
    fn send_user_message_steer_skips_remaining_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  let sent = false;
                  pi.on("tool_call", async (event) => {
                    if (sent) return {};
                    if (event && event.toolName === "count_tool") {
                      sent = true;
                      await pi.events("sendUserMessage", {
                        text: "steer-now",
                        options: { deliverAs: "steer" }
                      });
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(ToolUseProvider::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let _ = agent_session
                .run_text("go".to_string(), |_| {})
                .await
                .expect("run_text");

            // Steering interrupts outstanding tool execution for this turn.
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn send_user_message_follow_up_does_not_skip_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  let sent = false;
                  pi.on("tool_call", async (event) => {
                    if (sent) return {};
                    if (event && event.toolName === "count_tool") {
                      sent = true;
                      await pi.events("sendUserMessage", {
                        text: "follow-up",
                        options: { deliverAs: "followUp" }
                      });
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(ToolUseProvider::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let _ = agent_session
                .run_text("go".to_string(), |_| {})
                .await
                .expect("run_text");

            assert_eq!(calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn tool_call_hook_can_block_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    if (event && event.toolName === "count_tool") {
                      return { block: true, reason: "blocked in test" };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            assert_eq!(output.details, None);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "Tool execution blocked: blocked in test");
            }
        });
    }

    #[test]
    fn tool_call_hook_errors_fail_open() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_absent_allows_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r"
                export default function init(_pi) {}
                ",
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_returns_empty_allows_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => ({}));
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_can_block_bash_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    const name = event && event.toolName ? String(event.toolName) : "";
                    if (name === "bash") return { block: true, reason: "blocked bash in test" };
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&["bash"], temp_dir.path(), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&["bash"], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "printf 'hi' > blocked.txt" }),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(output.details, None);
            assert!(
                !temp_dir.path().join("blocked.txt").exists(),
                "expected bash command not to run when blocked"
            );
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "Tool execution blocked: blocked bash in test");
            }
        });
    }

    #[test]
    fn built_in_bash_denials_are_persisted_to_exec_mediation_and_alert_ledgers() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::from_tools(vec![Box::new(BashTool::with_shell_and_policy(
                temp_dir.path(),
                None,
                None,
                ExecMediationPolicy::default(),
                true,
                ReliabilityEnforcementMode::Hard,
            ))]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "rm -rf /" }),
                thought_signature: None,
            };
            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;
            assert!(is_error);
            assert!(output.is_error);

            let manager = agent_session
                .agent
                .extensions
                .clone()
                .expect("extensions manager");
            let exec_artifact = manager.exec_mediation_artifact();
            assert_eq!(exec_artifact.entry_count, 1);
            let exec_entry = exec_artifact.entries.last().expect("exec mediation entry");
            assert_eq!(exec_entry.extension_id.as_deref(), Some("builtin:bash"));
            assert_eq!(exec_entry.decision, "deny");
            assert!(
                matches!(
                    exec_entry.command_class.as_deref(),
                    Some("blocklisted" | "recursive_delete")
                ),
                "expected destructive command class, got {:?}",
                exec_entry.command_class
            );

            let alerts = manager.security_alert_artifact();
            let deny_alert = alerts
                .alerts
                .iter()
                .find(|alert| {
                    alert.extension_id == "builtin:bash"
                        && alert.category == SecurityAlertCategory::ExecMediation
                })
                .expect("deny alert");
            assert_eq!(deny_alert.severity, SecurityAlertSeverity::Error);
            assert_eq!(deny_alert.policy_source, "builtin_bash_exec_mediation");
        });
    }

    #[test]
    fn built_in_bash_audits_are_persisted_to_exec_mediation_and_alert_ledgers() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::from_tools(vec![Box::new(BashTool::with_shell_and_policy(
                temp_dir.path(),
                None,
                None,
                ExecMediationPolicy::default(),
                true,
                ReliabilityEnforcementMode::Observe,
            ))]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "chmod 777 ./__pi_missing_target__" }),
                thought_signature: None,
            };
            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (_output, _is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            let manager = agent_session
                .agent
                .extensions
                .clone()
                .expect("extensions manager");
            let exec_artifact = manager.exec_mediation_artifact();
            assert_eq!(exec_artifact.entry_count, 1);
            let exec_entry = exec_artifact.entries.last().expect("exec mediation entry");
            assert_eq!(exec_entry.extension_id.as_deref(), Some("builtin:bash"));
            assert_eq!(exec_entry.decision, "allow_with_audit");
            assert_eq!(
                exec_entry.command_class.as_deref(),
                Some("permission_escalation")
            );

            let alerts = manager.security_alert_artifact();
            let audit_alert = alerts
                .alerts
                .iter()
                .find(|alert| {
                    alert.extension_id == "builtin:bash"
                        && alert.category == SecurityAlertCategory::ExecMediation
                })
                .expect("audit alert");
            assert_eq!(audit_alert.severity, SecurityAlertSeverity::Info);
            assert_eq!(audit_alert.policy_source, "builtin_bash_exec_mediation");
        });
    }

    #[test]
    fn tool_result_hook_can_modify_tool_output() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (event) => {
                    if (event && event.toolName === "count_tool") {
                      return {
                        content: [{ type: "text", text: "modified" }],
                        details: { from: "tool_result" }
                      };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(output.details, Some(json!({ "from": "tool_result" })));

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "modified");
            }
        });
    }

    #[test]
    fn tool_result_hook_can_modify_tool_not_found_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (event) => {
                    if (event && event.toolName === "missing_tool" && event.isError) {
                      return {
                        content: [{ type: "text", text: "overridden" }],
                        details: { handled: true }
                      };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "missing_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(output.details, Some(json!({ "handled": true })));

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "overridden");
            }
        });
    }

    #[test]
    fn tool_result_hook_errors_fail_open() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            assert_eq!(output.details, None);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "ok");
            }
        });
    }

    #[test]
    fn tool_result_hook_runs_on_blocked_tool_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    if (event && event.toolName === "count_tool") {
                      return { block: true, reason: "blocked in test" };
                    }
                    return {};
                  });

                  pi.on("tool_result", async (event) => {
                    if (event && event.toolName === "count_tool" && event.isError) {
                      return { content: [{ type: "text", text: "override" }] };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session.agent.execute_tool(tool_call, on_event).await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "override");
            }
        });
    }
}

#[cfg(test)]
mod abort_tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context as TaskContext, Poll};

    struct StartThenPending {
        start: Option<StreamEvent>,
    }

    impl Stream for StartThenPending {
        type Item = crate::error::Result<StreamEvent>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Self::Item>> {
            if let Some(event) = self.start.take() {
                return Poll::Ready(Some(Ok(event)));
            }
            Poll::Pending
        }
    }

    #[derive(Debug)]
    struct HangingProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for HangingProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };

            Ok(Box::pin(StartThenPending {
                start: Some(StreamEvent::Start { partial }),
            }))
        }
    }

    #[derive(Debug)]
    struct CountingProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for CountingProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct PhasedProvider {
        pending_calls: usize,
        calls: AtomicUsize,
    }

    impl PhasedProvider {
        const fn new(pending_calls: usize) -> Self {
            Self {
                pending_calls,
                calls: AtomicUsize::new(0),
            }
        }

        fn base_message() -> AssistantMessage {
            AssistantMessage {
                content: Vec::new(),
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for PhasedProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.pending_calls {
                return Ok(Box::pin(StartThenPending {
                    start: Some(StreamEvent::Start {
                        partial: Self::base_message(),
                    }),
                }));
            }

            let partial = Self::base_message();
            let mut done = Self::base_message();
            done.content = vec![ContentBlock::Text(TextContent::new(format!(
                "resumed-response-{call}"
            )))];

            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: done,
                }),
            ])))
        }
    }

    #[derive(Debug)]
    struct ToolCallProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolCallProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let message = AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-1".to_string(),
                    name: "hanging_tool".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                })],
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: 0,
            };

            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::Done {
                    reason: StopReason::ToolUse,
                    message,
                },
            )])))
        }
    }

    #[derive(Debug)]
    struct HangingTool;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hanging_tool"
        }

        fn label(&self) -> &str {
            "Hanging Tool"
        }

        fn description(&self) -> &str {
            "Never completes unless aborted by the host"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> crate::error::Result<ToolOutput> {
            futures::future::pending::<()>().await;
            unreachable!("hanging tool should be aborted by the agent")
        }
    }

    fn event_tag(event: &AgentEvent) -> &'static str {
        match event {
            AgentEvent::AgentStart { .. } => "agent_start",
            AgentEvent::AgentEnd { error, .. } => {
                if error.as_deref() == Some("Aborted") {
                    "agent_end_aborted"
                } else {
                    "agent_end"
                }
            }
            AgentEvent::TurnStart { .. } => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event.as_ref() {
                AssistantMessageEvent::Error {
                    reason: StopReason::Aborted,
                    ..
                } => "assistant_error_aborted",
                AssistantMessageEvent::Done { .. } => "assistant_done",
                _ => "assistant_update",
            },
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_end",
            AgentEvent::AutoCompactionStart { .. } => "auto_compaction_start",
            AgentEvent::AutoCompactionEnd { .. } => "auto_compaction_end",
            AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
            AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
            AgentEvent::ExtensionError { .. } => "extension_error",
        }
    }

    fn assert_abort_resume_message_sequence(persisted: &[Message]) {
        assert_eq!(
            persisted.len(),
            6,
            "expected three user+assistant pairs, got: {persisted:?}"
        );

        let assistant_states = persisted
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.stop_reason),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            assistant_states,
            vec![StopReason::Aborted, StopReason::Aborted, StopReason::Stop]
        );
    }

    fn assert_abort_resume_timeline_boundaries(timeline: &[String]) {
        assert!(
            timeline
                .iter()
                .any(|event| event == "run0:agent_end_aborted"),
            "missing aborted boundary for first run: {timeline:?}"
        );
        assert!(
            timeline
                .iter()
                .any(|event| event == "run1:agent_end_aborted"),
            "missing aborted boundary for second run: {timeline:?}"
        );
        assert!(
            timeline.iter().any(|event| event == "run2:agent_end"),
            "missing successful boundary for resumed run: {timeline:?}"
        );
    }

    #[test]
    fn abort_interrupts_in_flight_stream() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let started = Arc::new(Notify::new());
        let started_wait = started.notified();

        let (abort_handle, abort_signal) = AbortHandle::new();

        let provider = Arc::new(HangingProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let started_tx = Arc::clone(&started);
        let join = handle.spawn(async move {
            agent_session
                .run_text_with_abort("hello".to_string(), Some(abort_signal), move |event| {
                    if matches!(
                        event,
                        AgentEvent::MessageStart {
                            message: Message::Assistant(_)
                        }
                    ) {
                        started_tx.notify_one();
                    }
                })
                .await
        });

        runtime.block_on(async move {
            started_wait.await;
            abort_handle.abort();

            let message = join.await.expect("run_text_with_abort");
            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(message.error_message.as_deref(), Some("Aborted"));
        });
    }

    #[test]
    fn abort_before_run_skips_provider_stream_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let (abort_handle, abort_signal) = AbortHandle::new();
        abort_handle.abort();

        runtime.block_on(async move {
            let message = agent_session
                .run_text_with_abort("hello".to_string(), Some(abort_signal), |_| {})
                .await
                .expect("run_text_with_abort");
            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn abort_then_resume_preserves_session_history() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(PhasedProvider::new(1));
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let started = Arc::new(Notify::new());
            let (abort_handle, abort_signal) = AbortHandle::new();
            let started_for_abort = Arc::clone(&started);
            let abort_join = handle.spawn(async move {
                started_for_abort.notified().await;
                abort_handle.abort();
            });

            let aborted = agent_session
                .run_text_with_abort("first".to_string(), Some(abort_signal), {
                    let started = Arc::clone(&started);
                    move |event| {
                        if matches!(
                            event,
                            AgentEvent::MessageStart {
                                message: Message::Assistant(_)
                            }
                        ) {
                            started.notify_one();
                        }
                    }
                })
                .await
                .expect("first run");
            abort_join.await;

            assert_eq!(aborted.stop_reason, StopReason::Aborted);
            assert_eq!(aborted.error_message.as_deref(), Some("Aborted"));

            let resumed = agent_session
                .run_text("second".to_string(), |_| {})
                .await
                .expect("resumed run");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert!(resumed.error_message.is_none());

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            assert_eq!(
                persisted.len(),
                4,
                "unexpected message history after abort+resume: {persisted:?}"
            );
            assert!(matches!(persisted.first(), Some(Message::User(_))));
            assert!(matches!(
                persisted.get(1),
                Some(Message::Assistant(assistant)) if assistant.stop_reason == StopReason::Aborted
            ));
            assert!(matches!(persisted.get(2), Some(Message::User(_))));
            assert!(matches!(
                persisted.get(3),
                Some(Message::Assistant(assistant))
                    if assistant.stop_reason == StopReason::Stop && assistant.error_message.is_none()
            ));
        });
    }

    #[test]
    fn repeated_abort_then_resume_has_consistent_timeline_and_state() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(PhasedProvider::new(2));
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let timeline = Arc::new(StdMutex::new(Vec::<String>::new()));

            for run_idx in 0..2 {
                let started = Arc::new(Notify::new());
                let (abort_handle, abort_signal) = AbortHandle::new();
                let started_for_abort = Arc::clone(&started);
                let abort_join = handle.spawn(async move {
                    started_for_abort.notified().await;
                    abort_handle.abort();
                });

                let run_timeline = Arc::clone(&timeline);
                let aborted = agent_session
                    .run_text_with_abort(format!("abort-run-{run_idx}"), Some(abort_signal), {
                        let started = Arc::clone(&started);
                        move |event| {
                            if let Ok(mut events) = run_timeline.lock() {
                                events.push(format!("run{run_idx}:{}", event_tag(&event)));
                            }
                            if matches!(
                                event,
                                AgentEvent::MessageStart {
                                    message: Message::Assistant(_)
                                }
                            ) {
                                started.notify_one();
                            }
                        }
                    })
                    .await
                    .expect("aborted run");
                abort_join.await;

                assert_eq!(
                    aborted.stop_reason,
                    StopReason::Aborted,
                    "run {run_idx} should abort cleanly"
                );
            }

            let run_timeline = Arc::clone(&timeline);
            let resumed = agent_session
                .run_text("final-run".to_string(), move |event| {
                    if let Ok(mut events) = run_timeline.lock() {
                        events.push(format!("run2:{}", event_tag(&event)));
                    }
                })
                .await
                .expect("final resumed run");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert!(resumed.error_message.is_none());

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            assert_abort_resume_message_sequence(&persisted);

            let timeline = timeline.lock().expect("timeline lock").clone();
            assert_abort_resume_timeline_boundaries(&timeline);
        });
    }

    #[test]
    fn abort_during_tool_execution_records_aborted_tool_result() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(ToolCallProvider);
            let tools = ToolRegistry::from_tools(vec![Box::new(HangingTool)]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let tool_started = Arc::new(Notify::new());
            let (abort_handle, abort_signal) = AbortHandle::new();
            let tool_started_for_abort = Arc::clone(&tool_started);
            let abort_join = handle.spawn(async move {
                tool_started_for_abort.notified().await;
                abort_handle.abort();
            });

            let result = agent_session
                .run_text_with_abort("trigger tool".to_string(), Some(abort_signal), {
                    let tool_started = Arc::clone(&tool_started);
                    move |event| {
                        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                            tool_started.notify_one();
                        }
                    }
                })
                .await
                .expect("tool-abort run");
            abort_join.await;
            assert_eq!(result.stop_reason, StopReason::Aborted);

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            let tool_result = persisted
                .iter()
                .find_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                })
                .expect("expected tool result message");
            assert!(tool_result.is_error);
            assert!(
                tool_result.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text(text) if text.text.contains("Tool execution aborted")
                    )
                }),
                "missing aborted tool marker in tool output: {:?}",
                tool_result.content
            );
        });
    }
}

#[cfg(test)]
mod turn_event_tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    // Note: Mutex from super::* is asupersync::sync::Mutex (for Session)
    // Use std::sync::Mutex directly for synchronous event capture

    fn assistant_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    struct SingleShotProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for SingleShotProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = assistant_message("");
            let final_message = assistant_message("hello");
            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[derive(Debug)]
    struct EchoTool;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn label(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "echo test tool"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("tool-ok"))],
                details: None,
                is_error: false,
            })
        }
    }

    #[derive(Debug)]
    struct ToolTurnProvider {
        calls: AtomicUsize,
    }

    impl ToolTurnProvider {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn assistant_message_with(
            &self,
            stop_reason: StopReason,
            content: Vec<ContentBlock>,
        ) -> AssistantMessage {
            AssistantMessage {
                content,
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolTurnProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            let partial = self.assistant_message_with(StopReason::Stop, Vec::new());
            let done = if call_index == 0 {
                self.assistant_message_with(
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "tool-1".to_string(),
                        name: "echo_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    })],
                )
            } else {
                self.assistant_message_with(
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new("final"))],
                )
            };

            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: done.stop_reason,
                    message: done,
                }),
            ])))
        }
    }

    #[test]
    fn turn_events_wrap_assistant_response() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(SingleShotProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture.lock().unwrap().push(event);
                })
                .await
                .expect("run_text")
        });

        runtime.block_on(async move {
            let message = join.await;
            assert_eq!(message.stop_reason, StopReason::Stop);

            let events = events.lock().unwrap();
            let turn_start_indices = events
                .iter()
                .enumerate()
                .filter_map(|(idx, event)| {
                    matches!(event, AgentEvent::TurnStart { .. }).then_some(idx)
                })
                .collect::<Vec<_>>();
            let turn_end_indices = events
                .iter()
                .enumerate()
                .filter_map(|(idx, event)| {
                    matches!(event, AgentEvent::TurnEnd { .. }).then_some(idx)
                })
                .collect::<Vec<_>>();

            assert_eq!(turn_start_indices.len(), 1);
            assert_eq!(turn_end_indices.len(), 1);
            assert!(turn_start_indices[0] < turn_end_indices[0]);

            let assistant_message_end = events
                .iter()
                .enumerate()
                .find_map(|(idx, event)| match event {
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(_),
                    } => Some(idx),
                    _ => None,
                })
                .expect("assistant message end");

            assert!(assistant_message_end < turn_end_indices[0]);

            let (message_is_assistant, tool_results_empty) = {
                let turn_end_event = &events[turn_end_indices[0]];
                assert!(
                    matches!(turn_end_event, AgentEvent::TurnEnd { .. }),
                    "Expected TurnEnd event, got {turn_end_event:?}"
                );
                match turn_end_event {
                    AgentEvent::TurnEnd {
                        message,
                        tool_results,
                        ..
                    } => (
                        matches!(message, Message::Assistant(_)),
                        tool_results.is_empty(),
                    ),
                    _ => (false, false),
                }
            };
            drop(events);
            assert!(message_is_assistant);
            assert!(tool_results_empty);
        });
    }

    #[test]
    fn turn_events_include_tool_execution_and_tool_result_messages() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(ToolTurnProvider::new());
        let tools = ToolRegistry::from_tools(vec![Box::new(EchoTool)]);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture.lock().expect("events lock").push(event);
                })
                .await
                .expect("run_text")
        });

        runtime.block_on(async move {
            let message = join.await;
            assert_eq!(message.stop_reason, StopReason::Stop);

            let events = events.lock().expect("events lock");
            let turn_start_count = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnStart { .. }))
                .count();
            let turn_end_count = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
                .count();
            assert_eq!(
                turn_start_count, 2,
                "expected one tool turn and one final turn"
            );
            assert_eq!(
                turn_end_count, 2,
                "expected one tool turn and one final turn"
            );

            let tool_start_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
                .expect("tool execution start event");
            let tool_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
                .expect("tool execution end event");
            assert!(tool_start_idx < tool_end_idx);

            let first_turn_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnEnd { turn_index: 0, .. }))
                .expect("first turn end");
            assert!(
                tool_end_idx < first_turn_end_idx,
                "tool execution should complete before first turn end"
            );

            let first_turn_tool_results = events.iter().find_map(|event| match event {
                AgentEvent::TurnEnd {
                    turn_index,
                    tool_results,
                    ..
                } if *turn_index == 0 => Some(tool_results),
                _ => None,
            });

            let Some(first_turn_tool_results) = first_turn_tool_results else {
                panic!("missing first turn tool results");
            };
            assert_eq!(first_turn_tool_results.len(), 1);
            let first_result = first_turn_tool_results.first().unwrap();
            if let Message::ToolResult(tr) = first_result {
                assert_eq!(tr.tool_name, "echo_tool");
                assert!(!tr.is_error);
            } else {
                panic!("expected ToolResult message");
            }
            drop(events);
        });
    }
}

impl AgentSession {
    pub const fn runtime_repair_mode_from_policy_mode(mode: RepairPolicyMode) -> RepairMode {
        match mode {
            RepairPolicyMode::Off => RepairMode::Off,
            RepairPolicyMode::Suggest => RepairMode::Suggest,
            RepairPolicyMode::AutoSafe => RepairMode::AutoSafe,
            RepairPolicyMode::AutoStrict => RepairMode::AutoStrict,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_js_extension_runtime(
        stage: &'static str,
        cwd: &std::path::Path,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
        policy: ExtensionPolicy,
        repair_mode: RepairMode,
        memory_limit_bytes: usize,
    ) -> Result<ExtensionRuntimeHandle> {
        let mut config = PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            repair_mode,
            ..PiJsRuntimeConfig::default()
        };
        config.limits.memory_limit_bytes = Some(memory_limit_bytes).filter(|bytes| *bytes > 0);

        let runtime =
            JsExtensionRuntimeHandle::start_with_policy(config, tools, manager, policy).await?;
        tracing::info!(
            event = "pi.extension_runtime.engine_decision",
            stage,
            requested = "quickjs",
            selected = "quickjs",
            fallback = false,
            "Extension runtime engine selected (legacy JS/TS)"
        );
        Ok(ExtensionRuntimeHandle::Js(runtime))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_native_extension_runtime(
        stage: &'static str,
        _cwd: &std::path::Path,
        _tools: Arc<ToolRegistry>,
        _manager: ExtensionManager,
        _policy: ExtensionPolicy,
        _repair_mode: RepairMode,
        _memory_limit_bytes: usize,
    ) -> Result<ExtensionRuntimeHandle> {
        let runtime = NativeRustExtensionRuntimeHandle::start().await?;
        tracing::info!(
            event = "pi.extension_runtime.engine_decision",
            stage,
            requested = "native-rust",
            selected = "native-rust",
            fallback = false,
            "Extension runtime engine selected (native-rust)"
        );
        Ok(ExtensionRuntimeHandle::NativeRust(runtime))
    }

    pub fn new(
        agent: Agent,
        session: Arc<Mutex<Session>>,
        save_enabled: bool,
        compaction_settings: ResolvedCompactionSettings,
    ) -> Self {
        Self {
            agent,
            session,
            save_enabled,
            retry_settings: RetrySettings::default(),
            reliability_enabled: false,
            reliability_enforcement_mode: ReliabilityEnforcementMode::Observe,
            extensions: None,
            extensions_is_streaming: Arc::new(AtomicBool::new(false)),
            compaction_settings,
            compaction_worker: CompactionWorkerState::new(CompactionQuota::default()),
            model_registry: None,
            auth_storage: None,
        }
    }

    #[must_use]
    pub const fn with_reliability_enabled(mut self, enabled: bool) -> Self {
        self.reliability_enabled = enabled;
        self
    }

    pub const fn set_reliability_enabled(&mut self, enabled: bool) {
        self.reliability_enabled = enabled;
    }

    pub const fn reliability_enabled(&self) -> bool {
        self.reliability_enabled
    }

    #[must_use]
    pub const fn with_reliability_mode(mut self, mode: ReliabilityEnforcementMode) -> Self {
        self.reliability_enforcement_mode = mode;
        self
    }

    pub const fn set_reliability_mode(&mut self, mode: ReliabilityEnforcementMode) {
        self.reliability_enforcement_mode = mode;
    }

    pub const fn reliability_mode(&self) -> ReliabilityEnforcementMode {
        self.reliability_enforcement_mode
    }

    pub const fn save_enabled(&self) -> bool {
        self.save_enabled
    }

    #[must_use]
    pub fn with_retry_settings(mut self, retry_settings: RetrySettings) -> Self {
        self.retry_settings = retry_settings;
        self
    }

    pub fn set_retry_settings(&mut self, retry_settings: RetrySettings) {
        self.retry_settings = retry_settings;
    }

    pub const fn retry_settings(&self) -> &RetrySettings {
        &self.retry_settings
    }

    pub const fn compaction_settings(&self) -> &ResolvedCompactionSettings {
        &self.compaction_settings
    }

    #[must_use]
    pub fn with_model_registry(mut self, registry: ModelRegistry) -> Self {
        self.model_registry = Some(registry);
        self
    }

    #[must_use]
    pub fn with_auth_storage(mut self, auth: AuthStorage) -> Self {
        self.auth_storage = Some(auth);
        self
    }

    pub fn set_model_registry(&mut self, registry: ModelRegistry) {
        self.model_registry = Some(registry);
    }

    pub fn set_auth_storage(&mut self, auth: AuthStorage) {
        self.auth_storage = Some(auth);
    }

    pub async fn set_provider_model(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.set_model_header(
                Some(provider_id.to_string()),
                Some(model_id.to_string()),
                None,
            );
        }

        self.apply_session_model_selection(provider_id, model_id);
        let provider = self.agent.provider();
        if provider.name() != provider_id || provider.model_id() != model_id {
            return Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}"
            )));
        }

        self.persist_session().await
    }

    fn resolve_stream_api_key_for_model(&self, entry: &ModelEntry) -> Option<String> {
        let normalize = |key_opt: Option<String>| {
            key_opt.and_then(|key| {
                let trimmed = key.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            })
        };

        self.auth_storage
            .as_ref()
            .and_then(|auth| {
                normalize(auth.resolve_api_key_for_model(
                    &entry.model.provider,
                    Some(&entry.model.id),
                    None,
                ))
            })
            .or_else(|| normalize(entry.api_key.clone()))
    }

    fn apply_session_model_selection(&mut self, provider_id: &str, model_id: &str) {
        let Some(registry) = &self.model_registry else {
            return;
        };

        let Some(entry) = registry.find(provider_id, model_id) else {
            tracing::warn!("Session model {provider_id}/{model_id} not found in model registry");
            return;
        };

        let provider_already_selected = self.agent.provider().name() == provider_id
            && self.agent.provider().model_id() == model_id;

        if !provider_already_selected {
            match crate::providers::create_provider(
                &entry,
                self.extensions.as_ref().map(ExtensionRegion::manager),
            ) {
                Ok(provider) => {
                    tracing::info!("Updating agent provider to {provider_id}/{model_id}");
                    self.agent.set_provider(provider);
                }
                Err(e) => {
                    tracing::warn!("Failed to create provider for session model: {e}");
                    return;
                }
            }
        }

        let resolved_key = self.resolve_stream_api_key_for_model(&entry);
        if resolved_key.is_none() {
            tracing::warn!(
                "No API key resolved for session model {provider_id}/{model_id}; clearing stream API key"
            );
        }

        let stream_options = self.agent.stream_options_mut();
        stream_options.api_key = resolved_key;
        stream_options.headers.clone_from(&entry.headers);
    }

    fn oauth_runtime_context(&self) -> Option<(String, String)> {
        let provider = self.agent.provider().name().trim().to_string();
        if provider.is_empty() {
            return None;
        }
        let api_key = self
            .agent
            .stream_options()
            .api_key
            .as_ref()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())?;
        Some((provider, api_key))
    }

    async fn record_oauth_runtime_outcome(
        &mut self,
        context: Option<(String, String)>,
        result: &Result<AssistantMessage>,
    ) {
        let Some((provider, api_key)) = context else {
            return;
        };
        let Some(auth) = self.auth_storage.as_mut() else {
            return;
        };

        match result {
            Ok(message)
                if !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) =>
            {
                auth.record_oauth_runtime_outcome(
                    provider.as_str(),
                    Some(api_key.as_str()),
                    crate::auth::OAuthRuntimeOutcome::Success,
                    None,
                );
            }
            Ok(message) => {
                let failure_reason =
                    message
                        .error_message
                        .as_deref()
                        .unwrap_or(match message.stop_reason {
                            StopReason::Error => "assistant returned stop_reason=error",
                            StopReason::Aborted => "assistant returned stop_reason=aborted",
                            _ => "assistant request failed",
                        });
                auth.record_oauth_runtime_outcome(
                    provider.as_str(),
                    Some(api_key.as_str()),
                    crate::auth::OAuthRuntimeOutcome::Failure,
                    Some(failure_reason),
                );
            }
            Err(error) => {
                auth.record_oauth_runtime_outcome(
                    provider.as_str(),
                    Some(api_key.as_str()),
                    crate::auth::OAuthRuntimeOutcome::Failure,
                    Some(&error.to_string()),
                );
            }
        }

        if let Err(error) = auth.save_async().await {
            tracing::warn!("Failed to persist OAuth runtime health state: {error}");
        }
    }

    fn reliability_task_id() -> String {
        format!("agent-turn-{}", Utc::now().timestamp_millis())
    }

    fn reliability_objective(raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return "Agent turn".to_string();
        }

        let mut out = String::new();
        for ch in trimmed.chars().take(RELIABILITY_OBJECTIVE_MAX_CHARS) {
            out.push(ch);
        }
        if trimmed.chars().count() > RELIABILITY_OBJECTIVE_MAX_CHARS {
            out.push_str("...");
        }
        out
    }

    fn reliability_artifact_store() -> Option<Arc<FsArtifactStore>> {
        let artifacts_root = Config::global_dir().join("reliability").join("artifacts");
        match FsArtifactStore::new(artifacts_root) {
            Ok(store) => Some(Arc::new(store)),
            Err(err) => {
                tracing::warn!(
                    "Failed to initialize reliability artifact store for agent trace capture: {err}"
                );
                None
            }
        }
    }

    fn collect_changed_test_paths() -> Vec<std::path::PathBuf> {
        let output = std::process::Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=ACMR"])
            .output();
        let Ok(output) = output else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(std::path::PathBuf::from)
            .filter(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext == "rs")
                    && (path.to_string_lossy().contains("test") || path.starts_with("tests"))
            })
            .collect()
    }

    fn collect_completion_evidence(snapshot: &ReliabilityTraceSnapshot) -> Vec<Evidence> {
        let mut collector = EvidenceCollector::with_auto_collect(false);

        for record in &snapshot.evidence {
            let summary = format!(
                "command='{}' exit={} stdout_hash={:?} stderr_hash={:?}",
                record.command, record.exit_code, record.stdout_hash, record.stderr_hash
            );
            let test_evidence = Evidence::TestOutput {
                passed: usize::from(record.exit_code == 0),
                failed: usize::from(record.exit_code != 0),
                output: summary,
            };
            collector.add(test_evidence);
        }

        if let Ok(git_evidence) = collector.collect_git_evidence() {
            collector.add(git_evidence);
        }

        collector.into_evidence()
    }

    #[allow(clippy::too_many_lines)]
    async fn append_reliability_trace_entries(
        &self,
        snapshot: ReliabilityTraceSnapshot,
        result: &Result<AssistantMessage>,
    ) -> Result<()> {
        if !self.reliability_enabled {
            return Ok(());
        }

        let (outcome, outcome_kind) = match result {
            Ok(message) => match message.stop_reason {
                StopReason::Stop => (
                    "Completed assistant stop".to_string(),
                    CloseOutcomeKind::Success,
                ),
                StopReason::Length => (
                    "Completed assistant length-capped response".to_string(),
                    CloseOutcomeKind::Success,
                ),
                StopReason::ToolUse => (
                    "Completed assistant tool request dispatch".to_string(),
                    CloseOutcomeKind::Success,
                ),
                StopReason::Error => (
                    "failed: assistant returned error stop reason".to_string(),
                    CloseOutcomeKind::Failure,
                ),
                StopReason::Aborted => (
                    "failed: assistant run was aborted".to_string(),
                    CloseOutcomeKind::Failure,
                ),
            },
            Err(err) => (
                format!("failed: agent run returned error: {err}"),
                CloseOutcomeKind::Failure,
            ),
        };

        let completion_evidence = Self::collect_completion_evidence(&snapshot);
        let mut task_events = snapshot.task_events.clone();
        if !snapshot.evidence.is_empty() {
            task_events.append(
                TaskEventKind::VerificationStarted,
                Some("agent".to_string()),
            );
        }

        let evidence_ids = snapshot
            .evidence
            .iter()
            .map(|record| record.evidence_id.clone())
            .collect::<Vec<_>>();

        let close_payload = ClosePayload {
            task_id: snapshot.task_id.clone(),
            outcome: outcome.clone(),
            outcome_kind: Some(outcome_kind),
            acceptance_ids: vec!["agent:turn_complete".to_string()],
            evidence_ids,
            trace_parent: Some(
                self.agent
                    .stream_options()
                    .session_id
                    .clone()
                    .filter(|session_id| !session_id.trim().is_empty())
                    .unwrap_or_else(|| format!("session:{}", snapshot.task_id)),
            ),
        };
        let phase_violations = snapshot.phase_violations.clone();
        let mut close_result = match CloseResult::evaluate(&close_payload, &snapshot.evidence, true)
        {
            Ok(result) => result,
            Err(err) => CloseResult {
                approved: false,
                violations: vec![err.to_string()],
            },
        };
        if !phase_violations.is_empty() {
            close_result.approved = false;
            close_result.violations.extend(phase_violations.clone());
        }

        if matches!(outcome_kind, CloseOutcomeKind::Success) {
            let evidence_gate = EvidenceGate::required();
            let evidence_result = evidence_gate.check_evidence(&completion_evidence);
            if evidence_result.is_fail() {
                close_result.approved = false;
                close_result
                    .violations
                    .push(evidence_result.message().to_string());
            }
        }

        let changed_test_paths = Self::collect_changed_test_paths();
        if !changed_test_paths.is_empty() {
            let test_quality_gate = TestQualityGate::new(changed_test_paths);
            match StateCompletionGate::check(&test_quality_gate) {
                Ok(gate_result) if gate_result.is_fail() => {
                    close_result.approved = false;
                    close_result
                        .violations
                        .push(gate_result.message().to_string());
                }
                Ok(_) => {}
                Err(err) => {
                    close_result.approved = false;
                    close_result
                        .violations
                        .push(format!("test quality gate error: {err}"));
                }
            }
        }

        let mut digest = StateDigest::new(
            snapshot.objective.clone(),
            if close_result.approved {
                "succeeded"
            } else {
                "failed"
            },
        );
        digest.recent_actions.push(format!(
            "execute_seen={}",
            if snapshot.saw_execute {
                "true"
            } else {
                "false"
            }
        ));
        digest
            .recent_actions
            .push(format!("evidence_count={}", snapshot.evidence.len()));
        digest
            .recent_actions
            .push(format!("phase_violation_count={}", phase_violations.len()));
        digest.recent_actions.push(format!(
            "stuck_pattern_count={}",
            snapshot.stuck_patterns.len()
        ));
        if let Some(summary) = &snapshot.discovery_summary {
            digest
                .recent_actions
                .push(format!("discovery_total={}", summary.total));
            digest
                .recent_actions
                .push(format!("discovery_unaddressed={}", summary.unaddressed));
            if summary.has_blocking() {
                close_result.approved = false;
                close_result.violations.push(format!(
                    "blocking discoveries present: {}",
                    summary.must_address_unaddressed
                ));
            }
        }
        if close_result.approved {
            digest.next_action = Some("Proceed to next task".to_string());
        } else {
            digest.blockers = close_result.violations.clone();
            digest.next_action = Some("Collect verification evidence and retry".to_string());
        }

        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;

        session.append_task_created_entry(
            snapshot.task_id.clone(),
            snapshot.objective,
            Some("agent".to_string()),
        );

        session.append_task_transition_entry(
            snapshot.task_id.clone(),
            None,
            "discover".to_string(),
            None,
        );
        session.append_task_transition_entry(
            snapshot.task_id.clone(),
            Some("discover".to_string()),
            "plan".to_string(),
            None,
        );

        let mut previous = "plan".to_string();
        if snapshot.saw_execute {
            session.append_task_transition_entry(
                snapshot.task_id.clone(),
                Some(previous.clone()),
                "execute".to_string(),
                None,
            );
            previous = "execute".to_string();
        }
        if !snapshot.evidence.is_empty() {
            session.append_task_transition_entry(
                snapshot.task_id.clone(),
                Some(previous.clone()),
                "verify".to_string(),
                Some(json!({
                    "evidenceCount": snapshot.evidence.len(),
                    "phaseViolationCount": phase_violations.len(),
                })),
            );
            previous = "verify".to_string();
        }

        if !snapshot.discoveries.is_empty() {
            session.append_custom_entry(
                "reliability_discoveries".to_string(),
                Some(json!({
                    "taskId": snapshot.task_id.clone(),
                    "discoveries": snapshot.discoveries.clone(),
                    "summary": snapshot.discovery_summary.clone(),
                })),
            );
        }

        for evidence in snapshot.evidence {
            session.append_verification_evidence_entry(evidence);
        }

        let completion_task_result = if close_result.approved {
            TaskResult::Success
        } else {
            TaskResult::Failed {
                reason: close_result.violations.join("; "),
            }
        };
        task_events.append(
            TaskEventKind::Completed {
                result: completion_task_result,
                reason: outcome.clone(),
                evidence: completion_evidence,
            },
            Some("agent".to_string()),
        );
        session.append_custom_entry(
            "task_event_log".to_string(),
            Some(json!({
                "taskId": snapshot.task_id.clone(),
                "events": task_events,
            })),
        );

        session.append_close_decision_entry(close_payload, close_result.clone());
        session.append_task_transition_entry(
            snapshot.task_id.clone(),
            Some(previous),
            "close".to_string(),
            Some(json!({
                "approved": close_result.approved,
                "violations": close_result.violations,
                "outcome": outcome,
                "phaseViolations": phase_violations,
            })),
        );
        session.append_state_digest_entry(digest);

        if self.save_enabled {
            session
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await?;
        }

        if matches!(
            self.reliability_enforcement_mode,
            ReliabilityEnforcementMode::Hard
        ) {
            if !snapshot.phase_violations.is_empty() {
                return Err(Error::validation(format!(
                    "reliability phase gates failed: {}",
                    snapshot.phase_violations.join("; ")
                )));
            }
            if !close_result.approved {
                return Err(Error::validation(format!(
                    "reliability completion gates failed: {}",
                    close_result.violations.join("; ")
                )));
            }
        }

        Ok(())
    }

    /// Force-run compaction synchronously (used by `/compact` slash command).
    pub async fn compact_now(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.compact_synchronous(Arc::new(on_event)).await
    }

    /// Two-phase non-blocking compaction.
    ///
    /// **Phase 1** — apply a completed background compaction result (if any).
    /// **Phase 2** — if quotas allow and the session needs compaction, start a
    /// new background compaction thread.
    async fn maybe_compact(&mut self, on_event: AgentEventHandler) -> Result<()> {
        if !self.compaction_settings.enabled {
            return Ok(());
        }

        // Phase 1: apply completed background result.
        if let Some(outcome) = self.compaction_worker.try_recv() {
            match outcome {
                Ok(result) => {
                    self.apply_compaction_result(result, Arc::clone(&on_event))
                        .await?;
                }
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                }
            }
        }

        // Phase 2: start new background compaction if quotas allow.
        if !self.compaction_worker.can_start() {
            return Ok(());
        }

        let preparation = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let entries = session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            compaction::prepare_compaction(&entries, self.compaction_settings.clone())
        };

        if let Some(prep) = preparation {
            on_event(AgentEvent::AutoCompactionStart {
                reason: "threshold".to_string(),
            });

            let provider = self.agent.provider();
            let api_key = self
                .agent
                .stream_options()
                .api_key
                .clone()
                .unwrap_or_default();

            self.compaction_worker.start(prep, provider, api_key, None);
        }

        Ok(())
    }

    /// Apply a completed compaction result to the session.
    async fn apply_compaction_result(
        &self,
        result: compaction::CompactionResult,
        on_event: AgentEventHandler,
    ) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;

        let details = compaction::compaction_details_to_value(&result.details).ok();
        let result_value = details.clone();

        session.append_compaction(
            result.summary,
            result.first_kept_entry_id,
            result.tokens_before,
            details,
            None, // from_hook
        );

        if self.save_enabled {
            session
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await?;
        }

        on_event(AgentEvent::AutoCompactionEnd {
            result: result_value,
            aborted: false,
            will_retry: false,
            error_message: None,
        });

        Ok(())
    }

    /// Run compaction synchronously (inline), blocking until completion.
    async fn compact_synchronous(&self, on_event: AgentEventHandler) -> Result<()> {
        if !self.compaction_settings.enabled {
            return Ok(());
        }

        let preparation = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let entries = session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            compaction::prepare_compaction(&entries, self.compaction_settings.clone())
        };

        if let Some(prep) = preparation {
            on_event(AgentEvent::AutoCompactionStart {
                reason: "threshold".to_string(),
            });

            let provider = self.agent.provider();
            let api_key = self
                .agent
                .stream_options()
                .api_key
                .clone()
                .unwrap_or_default();

            match compaction::compact(prep, provider, &api_key, None).await {
                Ok(result) => {
                    self.apply_compaction_result(result, Arc::clone(&on_event))
                        .await?;
                }
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enable_extensions(
        &mut self,
        enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
    ) -> Result<()> {
        self.enable_extensions_with_policy(
            enabled_tools,
            cwd,
            config,
            extension_entries,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn enable_extensions_with_policy(
        &mut self,
        enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
        policy: Option<ExtensionPolicy>,
        repair_policy: Option<RepairPolicyMode>,
        pre_warmed: Option<PreWarmedExtensionRuntime>,
    ) -> Result<()> {
        let mut js_specs: Vec<JsExtensionLoadSpec> = Vec::new();
        let mut native_specs: Vec<NativeRustExtensionLoadSpec> = Vec::new();
        #[cfg(feature = "wasm-host")]
        let mut wasm_specs: Vec<WasmExtensionLoadSpec> = Vec::new();

        for entry in extension_entries {
            match resolve_extension_load_spec(entry)? {
                ExtensionLoadSpec::Js(spec) => js_specs.push(spec),
                ExtensionLoadSpec::NativeRust(spec) => native_specs.push(spec),
                #[cfg(feature = "wasm-host")]
                ExtensionLoadSpec::Wasm(spec) => wasm_specs.push(spec),
            }
        }

        if !js_specs.is_empty() && !native_specs.is_empty() {
            return Err(Error::validation(
                "Mixed extension runtimes are not supported in one session yet. Use either JS/TS extensions (QuickJS) or native-rust descriptors (*.native.json), but not both at once."
                    .to_string(),
            ));
        }

        let resolved_policy = policy.clone().unwrap_or_default();
        let resolved_repair_policy = repair_policy
            .or_else(|| config.map(|cfg| cfg.resolve_repair_policy(None)))
            .unwrap_or(RepairPolicyMode::AutoSafe);
        let runtime_repair_mode =
            Self::runtime_repair_mode_from_policy_mode(resolved_repair_policy);
        let memory_limit_bytes =
            (resolved_policy.max_memory_mb as usize).saturating_mul(1024 * 1024);
        let wants_js_runtime = !js_specs.is_empty();

        // Either use the pre-warmed extension runtime (booted concurrently with startup)
        // or create a fresh runtime inline.
        #[allow(unused_variables)]
        let (manager, tools) = if let Some(pre) = pre_warmed {
            let manager = pre.manager;
            let tools = pre.tools;
            let runtime = match pre.runtime {
                ExtensionRuntimeHandle::NativeRust(runtime) => {
                    if wants_js_runtime {
                        tracing::warn!(
                            event = "pi.extension_runtime.prewarm.mismatch",
                            expected = "quickjs",
                            got = "native-rust",
                            "Pre-warmed runtime mismatched requested JS mode; creating quickjs runtime"
                        );
                        Self::start_js_extension_runtime(
                            "agent_enable_extensions_prewarm_mismatch",
                            cwd,
                            Arc::clone(&tools),
                            manager.clone(),
                            resolved_policy.clone(),
                            runtime_repair_mode,
                            memory_limit_bytes,
                        )
                        .await?
                    } else {
                        tracing::info!(
                            event = "pi.extension_runtime.engine_decision",
                            stage = "agent_enable_extensions_prewarmed",
                            requested = "native-rust",
                            selected = "native-rust",
                            fallback = false,
                            "Using pre-warmed extension runtime"
                        );
                        ExtensionRuntimeHandle::NativeRust(runtime)
                    }
                }
                ExtensionRuntimeHandle::Js(runtime) => {
                    if wants_js_runtime {
                        tracing::info!(
                            event = "pi.extension_runtime.engine_decision",
                            stage = "agent_enable_extensions_prewarmed",
                            requested = "quickjs",
                            selected = "quickjs",
                            fallback = false,
                            "Using pre-warmed extension runtime"
                        );
                        ExtensionRuntimeHandle::Js(runtime)
                    } else {
                        tracing::warn!(
                            event = "pi.extension_runtime.prewarm.mismatch",
                            expected = "native-rust",
                            got = "quickjs",
                            "Pre-warmed runtime mismatched requested native mode; creating native-rust runtime"
                        );
                        Self::start_native_extension_runtime(
                            "agent_enable_extensions_prewarm_mismatch",
                            cwd,
                            Arc::clone(&tools),
                            manager.clone(),
                            resolved_policy.clone(),
                            runtime_repair_mode,
                            memory_limit_bytes,
                        )
                        .await?
                    }
                }
            };
            manager.set_runtime(runtime);
            (manager, tools)
        } else {
            let manager = ExtensionManager::new();
            manager.set_cwd(cwd.display().to_string());
            let tools = Arc::new(ToolRegistry::new(enabled_tools, cwd, config));

            if let Some(cfg) = config {
                let resolved_risk = cfg.resolve_extension_risk_with_metadata();
                tracing::info!(
                    event = "pi.extension_runtime_risk.config",
                    source = resolved_risk.source,
                    enabled = resolved_risk.settings.enabled,
                    alpha = resolved_risk.settings.alpha,
                    window_size = resolved_risk.settings.window_size,
                    ledger_limit = resolved_risk.settings.ledger_limit,
                    fail_closed = resolved_risk.settings.fail_closed,
                    "Resolved extension runtime risk settings"
                );
                manager.set_runtime_risk_config(resolved_risk.settings);
            }

            let runtime = if wants_js_runtime {
                Self::start_js_extension_runtime(
                    "agent_enable_extensions_boot",
                    cwd,
                    Arc::clone(&tools),
                    manager.clone(),
                    resolved_policy,
                    runtime_repair_mode,
                    memory_limit_bytes,
                )
                .await?
            } else {
                Self::start_native_extension_runtime(
                    "agent_enable_extensions_boot",
                    cwd,
                    Arc::clone(&tools),
                    manager.clone(),
                    resolved_policy,
                    runtime_repair_mode,
                    memory_limit_bytes,
                )
                .await?
            };
            manager.set_runtime(runtime);
            (manager, tools)
        };

        // Session, host actions, and message fetchers are always set here
        // (after runtime boot) — the JS runtime only needs these when
        // dispatching hostcalls, which happens during extension loading.
        manager.set_session(Arc::new(SessionHandle(self.session.clone())));

        let injected = Arc::new(StdMutex::new(ExtensionInjectedQueue::default()));
        let host_actions = AgentSessionHostActions {
            session: Arc::clone(&self.session),
            injected: Arc::clone(&injected),
            is_streaming: Arc::clone(&self.extensions_is_streaming),
        };
        manager.set_host_actions(Arc::new(host_actions));
        {
            let steering_queue = Arc::clone(&injected);
            let follow_up_queue = Arc::clone(&injected);
            let steering_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
                let steering_queue = Arc::clone(&steering_queue);
                Box::pin(async move {
                    let Ok(mut queue) = steering_queue.lock() else {
                        return Vec::new();
                    };
                    queue.pop_steering()
                })
            };
            let follow_up_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
                let follow_up_queue = Arc::clone(&follow_up_queue);
                Box::pin(async move {
                    let Ok(mut queue) = follow_up_queue.lock() else {
                        return Vec::new();
                    };
                    queue.pop_follow_up()
                })
            };
            self.agent.register_message_fetchers(
                Some(Arc::new(steering_fetcher)),
                Some(Arc::new(follow_up_fetcher)),
            );
        }

        if !js_specs.is_empty() {
            manager.load_js_extensions(js_specs).await?;
        }

        if !native_specs.is_empty() {
            manager.load_native_extensions(native_specs).await?;
        }

        // Drain and log auto-repair diagnostics (bd-k5q5.8.11).
        if let Some(rt) = manager.runtime() {
            let events = rt.drain_repair_events().await;
            if !events.is_empty() {
                log_repair_diagnostics(&events);
            }
        }

        #[cfg(feature = "wasm-host")]
        if !wasm_specs.is_empty() {
            let host = WasmExtensionHost::new(cwd, policy.unwrap_or_default())?;
            manager
                .load_wasm_extensions(&host, wasm_specs, Arc::clone(&tools))
                .await?;
        }

        // Fire the `startup` lifecycle hook once extensions are loaded.
        // Fail-open: extension errors must not prevent the agent from running.
        let session_path = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::extension(e.to_string()))?;
            session.path.as_ref().map(|p| p.display().to_string())
        };

        if let Err(err) = manager
            .dispatch_event(
                ExtensionEventName::Startup,
                Some(serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "sessionFile": session_path,
                })),
            )
            .await
        {
            tracing::warn!("startup extension hook failed (fail-open): {err}");
        }

        let ctx_payload = serde_json::json!({ "cwd": cwd.display().to_string() });
        let wrappers = collect_extension_tool_wrappers(&manager, ctx_payload).await?;
        self.agent.extend_tools(wrappers);
        self.agent.extensions = Some(manager.clone());
        self.extensions = Some(ExtensionRegion::new(manager));
        Ok(())
    }

    pub async fn save_and_index(&mut self) -> Result<()> {
        if self.save_enabled {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await?;
        }
        Ok(())
    }

    pub async fn persist_session(&mut self) -> Result<()> {
        if !self.save_enabled {
            return Ok(());
        }
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        session
            .flush_autosave(AutosaveFlushTrigger::Periodic)
            .await?;
        Ok(())
    }

    pub async fn run_text(
        &mut self,
        input: String,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_text_with_abort(input, None, on_event).await
    }

    pub async fn run_text_with_abort(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let outcome = self.dispatch_input_event(input, Vec::new()).await?;
        let (text, images) = match outcome {
            InputEventOutcome::Continue { text, images } => (text, images),
            InputEventOutcome::Block { reason } => {
                let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                return Err(Error::extension(message));
            }
        };

        self.dispatch_before_agent_start().await;
        let on_event: AgentEventHandler = Arc::new(on_event);

        if images.is_empty() {
            self.run_with_retry(RetryInput::Text(text), abort, on_event)
                .await
        } else {
            let content = Self::build_content_blocks_for_input(&text, &images);
            self.run_with_retry(RetryInput::Content(content), abort, on_event)
                .await
        }
    }

    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_content_with_abort(content, None, on_event)
            .await
    }

    pub async fn run_with_content_with_abort(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let (text, images) = Self::split_content_blocks_for_input(&content);
        let outcome = self.dispatch_input_event(text, images).await?;
        let (text, images) = match outcome {
            InputEventOutcome::Continue { text, images } => (text, images),
            InputEventOutcome::Block { reason } => {
                let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                return Err(Error::extension(message));
            }
        };

        self.dispatch_before_agent_start().await;
        let on_event: AgentEventHandler = Arc::new(on_event);

        let content_for_agent = Self::build_content_blocks_for_input(&text, &images);
        self.run_with_retry(RetryInput::Content(content_for_agent), abort, on_event)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn run_with_retry(
        &mut self,
        input: RetryInput,
        abort: Option<AbortSignal>,
        on_event: AgentEventHandler,
    ) -> Result<AssistantMessage> {
        let max_attempts = self.retry_settings.max_retries.unwrap_or(3).max(1) as usize;
        let retry_enabled = self.retry_settings.is_enabled() && max_attempts > 1;
        if !retry_enabled {
            return self.run_single_attempt(input, abort, on_event).await;
        }

        let mut retry_agent = RetryAgent::new(RetryConfig {
            max_attempts,
            score_threshold: 0.7,
            best_of_n: true,
        });
        if let Some(token_budget) = self.retry_settings.token_budget {
            retry_agent = retry_agent.with_token_budget(token_budget);
        }

        let mut reviewer = ReviewerScorer::new();
        if let Some(model) = self.retry_settings.reviewer_model.clone() {
            reviewer = reviewer.with_model(model);
        }
        if let Some(token_budget) = self.retry_settings.token_budget {
            reviewer = reviewer.with_token_budget(token_budget);
        }

        let baseline = self.capture_retry_snapshot().await?;
        let original_save_enabled = self.save_enabled;
        self.save_enabled = false;

        let mut outcomes: Vec<RetryExecutionOutcome> = Vec::new();
        let mut attempt_index = 0usize;

        while attempt_index < max_attempts && retry_agent.check_token_budget() {
            if let Some(abort) = &abort {
                if abort.is_aborted() {
                    break;
                }
            }

            attempt_index += 1;
            self.restore_retry_snapshot(&baseline).await?;

            if attempt_index > 1 {
                on_event(AgentEvent::AutoRetryStart {
                    attempt: u32::try_from(attempt_index.saturating_sub(1)).unwrap_or(u32::MAX),
                    max_attempts: u32::try_from(max_attempts).unwrap_or(u32::MAX),
                    delay_ms: 0,
                    error_message: "running additional attempt for best-of-N selection".to_string(),
                });
            }

            let started = Instant::now();
            let result = self
                .run_single_attempt(input.clone(), abort.clone(), Arc::clone(&on_event))
                .await;
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let snapshot = self.capture_retry_snapshot().await?;

            let verification = if Self::result_is_success(&result) {
                VerificationOutcome::Passed
            } else {
                VerificationOutcome::Failed
            };
            let token_usage = result
                .as_ref()
                .ok()
                .map(|message| TokenUsage::new(message.usage.input, message.usage.output));

            let mut attempt = Attempt::from_patch(Self::attempt_patch(&result))
                .with_verification(verification)
                .with_stats(0, 0, 0)
                .with_notes(format!("attempt {attempt_index}/{max_attempts}"));
            if let Some(tokens) = token_usage.clone() {
                attempt = attempt.with_token_usage(tokens);
            }
            if let Some(err) = Self::result_error(&result) {
                attempt = attempt.with_error(err);
            }

            let submission = ReviewSubmission {
                attempt_id: attempt.id.clone(),
                trajectory: vec![
                    format!("attempt_index={attempt_index}"),
                    format!(
                        "result={}",
                        if Self::result_is_success(&result) {
                            "success"
                        } else {
                            "failure"
                        }
                    ),
                ],
                stats: AttemptStats {
                    files_changed: 0,
                    lines_added: 0,
                    lines_removed: 0,
                    tests_passed: usize::from(verification == VerificationOutcome::Passed),
                    tests_failed: usize::from(verification == VerificationOutcome::Failed),
                    duration_ms,
                },
                token_usage,
            };
            let review_score = reviewer.score(&submission) / 100.0;
            attempt = attempt.with_score(review_score.clamp(0.0, 1.0));
            let attempt_id = attempt.id.clone();
            retry_agent.record_attempt(attempt);

            outcomes.push(RetryExecutionOutcome {
                attempt_id,
                snapshot,
                result,
            });

            if !retry_agent.can_retry() {
                break;
            }
        }

        if outcomes.is_empty() {
            self.save_enabled = original_save_enabled;
            return Err(Error::validation(
                "retry enabled but no attempts were executed",
            ));
        }

        let selected_id = retry_agent.final_result().map(|attempt| attempt.id.clone());
        let selected_index = selected_id
            .as_ref()
            .and_then(|attempt_id| {
                outcomes
                    .iter()
                    .position(|candidate| &candidate.attempt_id == attempt_id)
            })
            .unwrap_or(outcomes.len() - 1);

        let selected = outcomes.swap_remove(selected_index);
        let success = Self::result_is_success(&selected.result);
        let final_error = if success {
            None
        } else {
            Self::result_error(&selected.result)
        };

        self.restore_retry_snapshot(&selected.snapshot).await?;
        self.save_enabled = original_save_enabled;
        if self.save_enabled {
            self.persist_session().await?;
        }

        if attempt_index > 1 {
            on_event(AgentEvent::AutoRetryEnd {
                success,
                attempt: u32::try_from(attempt_index.saturating_sub(1)).unwrap_or(u32::MAX),
                final_error,
            });
        }

        selected.result
    }

    async fn run_single_attempt(
        &mut self,
        input: RetryInput,
        abort: Option<AbortSignal>,
        on_event: AgentEventHandler,
    ) -> Result<AssistantMessage> {
        match input {
            RetryInput::Text(text) => {
                let handler = Arc::clone(&on_event);
                self.run_agent_with_text(text, abort, move |event| handler(event))
                    .await
            }
            RetryInput::Content(content) => {
                let handler = Arc::clone(&on_event);
                self.run_agent_with_content(content, abort, move |event| handler(event))
                    .await
            }
        }
    }

    async fn capture_retry_snapshot(&self) -> Result<RetryExecutionSnapshot> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        Ok(RetryExecutionSnapshot {
            session: session.clone(),
            agent_messages: self.agent.messages().to_vec(),
        })
    }

    async fn restore_retry_snapshot(&mut self, snapshot: &RetryExecutionSnapshot) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        *session = snapshot.session.clone();
        self.agent.replace_messages(snapshot.agent_messages.clone());
        Ok(())
    }

    fn result_is_success(result: &Result<AssistantMessage>) -> bool {
        result.as_ref().is_ok_and(|message| {
            !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        })
    }

    fn result_error(result: &Result<AssistantMessage>) -> Option<String> {
        match result {
            Ok(message)
                if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) =>
            {
                message
                    .error_message
                    .clone()
                    .or_else(|| Some("assistant returned an error stop reason".to_string()))
            }
            Ok(_) => None,
            Err(err) => Some(err.to_string()),
        }
    }

    fn attempt_patch(result: &Result<AssistantMessage>) -> String {
        match result {
            Ok(message) => message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(err) => format!("attempt failed: {err}"),
        }
    }

    async fn dispatch_input_event(
        &self,
        text: String,
        images: Vec<ImageContent>,
    ) -> Result<InputEventOutcome> {
        let Some(region) = &self.extensions else {
            return Ok(InputEventOutcome::Continue { text, images });
        };

        let images_value = serde_json::to_value(&images).unwrap_or(Value::Null);
        let payload = json!({
            "text": text,
            "images": images_value,
            "source": "user",
        });

        let response = region
            .manager()
            .dispatch_event_with_response(
                ExtensionEventName::Input,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await?;

        Ok(apply_input_event_response(response, text, images))
    }

    async fn dispatch_before_agent_start(&self) {
        if let Some(region) = &self.extensions {
            if let Err(err) = region
                .manager()
                .dispatch_event(ExtensionEventName::BeforeAgentStart, None)
                .await
            {
                tracing::warn!("before_agent_start extension hook failed (fail-open): {err}");
            }
        }
    }

    fn split_content_blocks_for_input(blocks: &[ContentBlock]) -> (String, Vec<ImageContent>) {
        let mut text = String::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                ContentBlock::Text(text_block) => {
                    if !text_block.text.trim().is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&text_block.text);
                    }
                }
                ContentBlock::Image(image) => images.push(image.clone()),
                _ => {}
            }
        }
        (text, images)
    }

    fn build_content_blocks_for_input(text: &str, images: &[ImageContent]) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        if !text.trim().is_empty() {
            content.push(ContentBlock::Text(TextContent::new(text.to_string())));
        }
        for image in images {
            content.push(ContentBlock::Image(image.clone()));
        }
        content
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run_agent_with_text(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
        let session_model = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            (
                session.header.provider.clone(),
                session.header.model_id.clone(),
            )
        };

        if let (Some(provider_id), Some(model_id)) = session_model {
            self.apply_session_model_selection(provider_id.as_str(), model_id.as_str());
        }
        let oauth_context = self.oauth_runtime_context();

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();
        let reliability_objective = Self::reliability_objective(&input);
        let reliability_artifact_store = self
            .reliability_enabled
            .then(Self::reliability_artifact_store)
            .flatten();
        if self.reliability_enabled {
            self.agent
                .set_scope_objective(Some(reliability_objective.clone()));
        } else {
            self.agent.clear_scope_objective();
        }

        // Create and persist user message immediately to avoid data loss on API errors
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(input),
            timestamp: Utc::now().timestamp_millis(),
        });

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.append_model_message(user_message.clone());
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
            }
        }

        let reliability_capture = self.reliability_enabled.then(|| {
            Arc::new(StdMutex::new(ReliabilityTraceCapture::new(
                Self::reliability_task_id(),
                reliability_objective,
                Some(format!(
                    "{}/{}",
                    self.agent.provider().name(),
                    self.agent.provider().model_id()
                )),
                reliability_artifact_store.clone(),
            )))
        });

        self.extensions_is_streaming.store(true, Ordering::SeqCst);
        let on_event_for_run = Arc::clone(&on_event);
        let capture_for_run = reliability_capture.clone();
        let result = self
            .agent
            .run_with_message_with_abort(user_message, abort, move |event| {
                if let Some(capture) = &capture_for_run {
                    if let Ok(mut guard) = capture.lock() {
                        guard.observe_event(&event);
                    }
                }
                on_event_for_run(event);
            })
            .await;
        self.agent.clear_scope_objective();
        self.extensions_is_streaming.store(false, Ordering::SeqCst);
        self.record_oauth_runtime_outcome(oauth_context, &result)
            .await;
        if let Some(capture) = reliability_capture {
            let snapshot = capture.lock().ok().map(|guard| guard.snapshot());
            if let Some(snapshot) = snapshot {
                if let Err(err) = self
                    .append_reliability_trace_entries(snapshot, &result)
                    .await
                {
                    if matches!(
                        self.reliability_enforcement_mode,
                        ReliabilityEnforcementMode::Hard
                    ) {
                        return Err(err);
                    }
                    tracing::warn!("Failed to append reliability trace entries: {err}");
                }
            }
        }
        let result = result?;
        // Persist only NEW messages (assistant/tools), skipping the user message we already saved.
        self.persist_new_messages(start_len + 1).await?;
        Ok(result)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run_agent_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
        let session_model = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            (
                session.header.provider.clone(),
                session.header.model_id.clone(),
            )
        };

        if let (Some(provider_id), Some(model_id)) = session_model {
            self.apply_session_model_selection(provider_id.as_str(), model_id.as_str());
        }
        let oauth_context = self.oauth_runtime_context();

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();
        let (objective_text, _) = Self::split_content_blocks_for_input(&content);
        let reliability_objective = Self::reliability_objective(&objective_text);
        let reliability_artifact_store = self
            .reliability_enabled
            .then(Self::reliability_artifact_store)
            .flatten();
        if self.reliability_enabled {
            self.agent
                .set_scope_objective(Some(reliability_objective.clone()));
        } else {
            self.agent.clear_scope_objective();
        }

        // Create and persist user message immediately to avoid data loss on API errors
        let user_message = Message::User(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: Utc::now().timestamp_millis(),
        });

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.append_model_message(user_message.clone());
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
            }
        }

        let reliability_capture = self.reliability_enabled.then(|| {
            Arc::new(StdMutex::new(ReliabilityTraceCapture::new(
                Self::reliability_task_id(),
                reliability_objective,
                Some(format!(
                    "{}/{}",
                    self.agent.provider().name(),
                    self.agent.provider().model_id()
                )),
                reliability_artifact_store.clone(),
            )))
        });

        self.extensions_is_streaming.store(true, Ordering::SeqCst);
        let on_event_for_run = Arc::clone(&on_event);
        let capture_for_run = reliability_capture.clone();
        let result = self
            .agent
            .run_with_message_with_abort(user_message, abort, move |event| {
                if let Some(capture) = &capture_for_run {
                    if let Ok(mut guard) = capture.lock() {
                        guard.observe_event(&event);
                    }
                }
                on_event_for_run(event);
            })
            .await;
        self.agent.clear_scope_objective();
        self.extensions_is_streaming.store(false, Ordering::SeqCst);
        self.record_oauth_runtime_outcome(oauth_context, &result)
            .await;
        if let Some(capture) = reliability_capture {
            let snapshot = capture.lock().ok().map(|guard| guard.snapshot());
            if let Some(snapshot) = snapshot {
                if let Err(err) = self
                    .append_reliability_trace_entries(snapshot, &result)
                    .await
                {
                    if matches!(
                        self.reliability_enforcement_mode,
                        ReliabilityEnforcementMode::Hard
                    ) {
                        return Err(err);
                    }
                    tracing::warn!("Failed to append reliability trace entries: {err}");
                }
            }
        }
        let result = result?;
        // Persist only NEW messages (assistant/tools), skipping the user message we already saved.
        self.persist_new_messages(start_len + 1).await?;
        Ok(result)
    }

    async fn persist_new_messages(&self, start_len: usize) -> Result<()> {
        let new_messages = self.agent.messages()[start_len..].to_vec();
        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            for message in new_messages {
                session.append_model_message(message);
            }
            if self.save_enabled {
                session
                    .flush_autosave(AutosaveFlushTrigger::Periodic)
                    .await?;
            }
        }
        Ok(())
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Log a summary of auto-repair events that fired during extension loading.
///
/// Default: one-line summary.  Set `PI_AUTO_REPAIR_VERBOSE=1` for per-extension
/// detail.  Structured tracing events are always emitted regardless of verbosity.
fn log_repair_diagnostics(events: &[crate::extensions_js::ExtensionRepairEvent]) {
    use std::collections::BTreeMap;

    // Always emit structured tracing events for each repair.
    for ev in events {
        tracing::info!(
            event = "extension.auto_repair",
            extension_id = %ev.extension_id,
            pattern = %ev.pattern,
            success = ev.success,
            original_error = %ev.original_error,
            repair_action = %ev.repair_action,
        );
    }

    // Group by pattern for the summary line.
    let mut by_pattern: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for ev in events {
        by_pattern
            .entry(ev.pattern.to_string())
            .or_default()
            .push(&ev.extension_id);
    }

    let verbose = std::env::var("PI_AUTO_REPAIR_VERBOSE")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    if verbose {
        eprintln!(
            "[auto-repair] {} extension{} auto-repaired:",
            events.len(),
            if events.len() == 1 { "" } else { "s" }
        );
        for ev in events {
            eprintln!(
                "  {}: {} ({})",
                ev.pattern, ev.extension_id, ev.repair_action
            );
        }
    } else {
        // Compact one-line summary.
        let patterns: Vec<String> = by_pattern
            .iter()
            .map(|(pat, ids)| format!("{pat}:{}", ids.len()))
            .collect();
        tracing::info!(
            event = "extension.auto_repair.summary",
            count = events.len(),
            patterns = %patterns.join(", "),
            "auto-repaired {} extension(s)",
            events.len(),
        );
    }
}

const BLOCK_IMAGES_PLACEHOLDER: &str = "Image reading is disabled.";

#[derive(Debug, Default, Clone, Copy)]
struct ImageFilterStats {
    removed_images: usize,
    affected_messages: usize,
}

fn filter_images_for_provider(messages: &mut [Message]) -> ImageFilterStats {
    let mut stats = ImageFilterStats::default();
    for message in messages {
        let removed = filter_images_from_message(message);
        if removed > 0 {
            stats.removed_images += removed;
            stats.affected_messages += 1;
        }
    }
    stats
}

fn filter_images_from_message(message: &mut Message) -> usize {
    match message {
        Message::User(user) => match &mut user.content {
            UserContent::Text(_) => 0,
            UserContent::Blocks(blocks) => filter_image_blocks(blocks),
        },
        Message::Assistant(assistant) => {
            let assistant = Arc::make_mut(assistant);
            filter_image_blocks(&mut assistant.content)
        }
        Message::ToolResult(tool_result) => {
            filter_image_blocks(&mut Arc::make_mut(tool_result).content)
        }
        Message::Custom(_) => 0,
    }
}

fn filter_image_blocks(blocks: &mut Vec<ContentBlock>) -> usize {
    let mut removed = 0usize;
    let mut filtered = Vec::with_capacity(blocks.len());

    for block in blocks.drain(..) {
        match block {
            ContentBlock::Image(_) => {
                removed += 1;
                let previous_is_placeholder =
                    filtered
                        .last()
                        .is_some_and(|prev| matches!(prev, ContentBlock::Text(TextContent { text, .. }) if text == BLOCK_IMAGES_PLACEHOLDER));
                if !previous_is_placeholder {
                    filtered.push(ContentBlock::Text(TextContent::new(
                        BLOCK_IMAGES_PLACEHOLDER,
                    )));
                }
            }
            other => filtered.push(other),
        }
    }

    *blocks = filtered;
    removed
}

/// Extract tool calls from content blocks.
fn extract_tool_calls(content: &[ContentBlock]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::ToolCall(tc) = block {
                Some(tc.clone())
            } else {
                None
            }
        })
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthCredential;
    use crate::provider::{InputType, Model, ModelCost};
    use crate::reliability::{ArtifactQuery, ArtifactStore, FsArtifactStore};
    use async_trait::async_trait;
    use futures::Stream;
    use std::collections::HashMap;
    use std::path::Path;
    use std::pin::Pin;

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn assert_user_text(message: &Message, expected: &str) {
        assert!(
            matches!(
                message,
                Message::User(UserMessage {
                    content: UserContent::Text(_),
                    ..
                })
            ),
            "expected user text message, got {message:?}"
        );
        if let Message::User(UserMessage {
            content: UserContent::Text(text),
            ..
        }) = message
        {
            assert_eq!(text, expected);
        }
    }

    fn sample_image_block() -> ContentBlock {
        ContentBlock::Image(ImageContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
        })
    }

    fn image_count_in_message(message: &Message) -> usize {
        let count_images = |blocks: &[ContentBlock]| {
            blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image(_)))
                .count()
        };
        match message {
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) => count_images(blocks),
            Message::Assistant(msg) => count_images(&msg.content),
            Message::ToolResult(tool_result) => count_images(&tool_result.content),
            Message::User(UserMessage {
                content: UserContent::Text(_),
                ..
            })
            | Message::Custom(_) => 0,
        }
    }

    #[derive(Debug)]
    struct SilentProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for SilentProvider {
        fn name(&self) -> &str {
            "silent-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[test]
    fn test_extract_tool_calls() {
        let content = vec![
            ContentBlock::Text(TextContent::new("Hello")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"path": "file.txt"}),
                thought_signature: None,
            }),
            ContentBlock::Text(TextContent::new("World")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc2".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            }),
        ];

        let tool_calls = extract_tool_calls(&content);
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].name, "read");
        assert_eq!(tool_calls[1].name, "bash");
    }

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.max_tool_iterations, 50);
        assert!(config.system_prompt.is_none());
        assert!(!config.block_images);
    }

    #[test]
    fn scope_guard_blocks_out_of_scope_mutation_for_explicit_objective() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&["write"], Path::new("."), None),
            AgentConfig::default(),
        );
        agent.set_scope_objective(Some("Update src/agent.rs retry handling".to_string()));

        let out_of_scope_call = ToolCall {
            id: "tc-scope-1".to_string(),
            name: "write".to_string(),
            arguments: serde_json::json!({
                "file_path": "src/main.rs",
                "content": "fn main() {}",
            }),
            thought_signature: None,
        };

        assert!(agent.should_block_tool_call_for_scope(&out_of_scope_call));

        let output = agent.scope_blocked_tool_output(&out_of_scope_call);
        assert!(output.is_error);
        assert_eq!(
            output
                .details
                .as_ref()
                .and_then(|details| details.get("scopeBlocked"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn scope_guard_does_not_block_broad_objective() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&["write"], Path::new("."), None),
            AgentConfig::default(),
        );
        agent.set_scope_objective(Some("Implement the plan".to_string()));

        let call = ToolCall {
            id: "tc-scope-2".to_string(),
            name: "write".to_string(),
            arguments: serde_json::json!({
                "file_path": "src/main.rs",
                "content": "fn main() {}",
            }),
            thought_signature: None,
        };

        assert!(!agent.should_block_tool_call_for_scope(&call));
    }

    #[test]
    fn filter_image_blocks_replaces_images_with_deduped_placeholder_text() {
        let mut blocks = vec![
            sample_image_block(),
            sample_image_block(),
            ContentBlock::Text(TextContent::new("tail")),
            sample_image_block(),
        ];

        let removed = filter_image_blocks(&mut blocks);

        assert_eq!(removed, 3);
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
        assert!(matches!(
            blocks.first(),
            Some(ContentBlock::Text(TextContent { text, .. })) if text == BLOCK_IMAGES_PLACEHOLDER
        ));
        assert!(matches!(
            blocks.get(1),
            Some(ContentBlock::Text(TextContent { text, .. })) if text == "tail"
        ));
        assert!(matches!(
            blocks.get(2),
            Some(ContentBlock::Text(TextContent { text, .. })) if text == BLOCK_IMAGES_PLACEHOLDER
        ));
    }

    #[test]
    fn filter_images_for_provider_filters_images_from_all_block_message_types() {
        let mut messages = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new("hello")),
                    sample_image_block(),
                ]),
                timestamp: 0,
            }),
            Message::Assistant(Arc::new(AssistantMessage {
                content: vec![sample_image_block()],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            })),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "tc1".to_string(),
                tool_name: "read".to_string(),
                content: vec![
                    sample_image_block(),
                    ContentBlock::Text(TextContent::new("ok")),
                ],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];

        let stats = filter_images_for_provider(&mut messages);

        assert_eq!(stats.removed_images, 3);
        assert_eq!(stats.affected_messages, 3);
        assert_eq!(
            messages.iter().map(image_count_in_message).sum::<usize>(),
            0,
            "no images should remain in provider-bound context"
        );
    }

    #[test]
    fn build_context_strips_images_when_block_images_enabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: true,
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 0);
        assert!(matches!(
            &context.messages[0],
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) if blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text(TextContent { text, .. }) if text == BLOCK_IMAGES_PLACEHOLDER))
        ));
    }

    #[test]
    fn build_context_keeps_images_when_block_images_disabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: false,
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 1);
    }

    #[test]
    fn auto_compaction_start_serializes_with_pi_mono_compatible_type_tag() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_with_pi_mono_compatible_fields() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({"tokens_before": 5000, "tokens_after": 2000})),
            aborted: false,
            will_retry: false,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], false);
        assert!(json.get("errorMessage").is_none()); // skipped when None
        assert!(json["result"].is_object());
    }

    #[test]
    fn auto_compaction_end_includes_error_message_when_present() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: true,
            will_retry: false,
            error_message: Some("Compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], true);
        assert_eq!(json["errorMessage"], "Compaction failed");
    }

    #[test]
    fn auto_retry_start_serializes_with_camel_case_fields() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "Rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 2000);
        assert_eq!(json["errorMessage"], "Rate limited");
    }

    #[test]
    fn auto_retry_end_serializes_success_and_omits_null_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_includes_final_error_on_failure() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("Max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "Max retries exceeded");
    }

    #[test]
    fn message_queue_push_increments_seq_and_counts_both_queues() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        assert_eq!(queue.pending_count(), 0);

        assert_eq!(queue.push_steering(user_message("s1")), 0);
        assert_eq!(queue.push_follow_up(user_message("f1")), 1);
        assert_eq!(queue.push_steering(user_message("s2")), 2);

        assert_eq!(queue.pending_count(), 3);
    }

    #[test]
    fn message_queue_pop_steering_one_at_a_time_preserves_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("s1"));
        queue.push_steering(user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(&first[0], "s1");
        assert_eq!(queue.pending_count(), 1);

        let second = queue.pop_steering();
        assert_eq!(second.len(), 1);
        assert_user_text(&second[0], "s2");
        assert_eq!(queue.pending_count(), 0);

        let empty = queue.pop_steering();
        assert!(empty.is_empty());
    }

    #[test]
    fn message_queue_pop_respects_queue_modes_per_kind() {
        let mut queue = MessageQueue::new(QueueMode::All, QueueMode::OneAtATime);
        queue.push_steering(user_message("s1"));
        queue.push_steering(user_message("s2"));
        queue.push_follow_up(user_message("f1"));
        queue.push_follow_up(user_message("f2"));

        let steering = queue.pop_steering();
        assert_eq!(steering.len(), 2);
        assert_user_text(&steering[0], "s1");
        assert_user_text(&steering[1], "s2");
        assert_eq!(queue.pending_count(), 2);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(&follow_up[0], "f1");
        assert_eq!(queue.pending_count(), 1);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(&follow_up[0], "f2");
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn message_queue_set_modes_applies_to_existing_messages() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(user_message("s1"));
        queue.push_steering(user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(&first[0], "s1");

        queue.set_modes(QueueMode::All, QueueMode::OneAtATime);
        let remaining = queue.pop_steering();
        assert_eq!(remaining.len(), 1);
        assert_user_text(&remaining[0], "s2");
    }

    fn build_switch_test_session(auth: &AuthStorage) -> AgentSession {
        let registry = ModelRegistry::load(auth, None);
        let current_entry = registry
            .find("anthropic", "claude-sonnet-4-6")
            .expect("anthropic model in registry");
        let provider = crate::providers::create_provider(&current_entry, None)
            .expect("create anthropic provider");
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut stream_options = StreamOptions {
            api_key: Some("stale-key".to_string()),
            ..Default::default()
        };
        let _ = stream_options
            .headers
            .insert("x-stale-header".to_string(), "stale-value".to_string());
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options,
                block_images: false,
            },
        );

        let mut session = Session::in_memory();
        session.header.provider = Some("openai".to_string());
        session.header.model_id = Some("gpt-4o".to_string());

        let mut agent_session = AgentSession::new(
            agent,
            Arc::new(Mutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        );
        agent_session.set_model_registry(registry);
        agent_session.set_auth_storage(auth.clone());
        agent_session
    }

    #[test]
    fn oauth_pool_outcome() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build();
        runtime.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let expiry = chrono::Utc::now().timestamp_millis() + 60_000;

            let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
            auth.set_login_credential(
                "openai",
                AuthCredential::OAuth {
                    access_token: "pool-token-a".to_string(),
                    refresh_token: "pool-refresh-a".to_string(),
                    expires: expiry,
                    token_url: None,
                    client_id: None,
                },
            );
            auth.set_login_credential(
                "openai",
                AuthCredential::OAuth {
                    access_token: "pool-token-b".to_string(),
                    refresh_token: "pool-refresh-b".to_string(),
                    expires: expiry,
                    token_url: None,
                    client_id: None,
                },
            );
            auth.save().expect("save auth");

            let registry = ModelRegistry::load(&auth, None);
            let openai_entry = registry
                .find("openai", "gpt-4o")
                .expect("openai model in registry");
            let provider =
                crate::providers::create_provider(&openai_entry, None).expect("create provider");
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    system_prompt: None,
                    max_tool_iterations: 50,
                    stream_options: StreamOptions::default(),
                    block_images: false,
                },
            );
            let mut session = Session::in_memory();
            session.header.provider = Some("openai".to_string());
            session.header.model_id = Some("gpt-4o".to_string());
            let mut agent_session = AgentSession::new(
                agent,
                Arc::new(Mutex::new(session)),
                false,
                ResolvedCompactionSettings::default(),
            );
            agent_session.set_model_registry(registry);
            agent_session.set_auth_storage(auth.clone());
            agent_session.apply_session_model_selection("openai", "gpt-4o");
            let selected_token = agent_session
                .agent
                .stream_options()
                .api_key
                .clone()
                .expect("selected oauth token");

            let failure_result: Result<AssistantMessage> = Ok(AssistantMessage {
                content: vec![],
                api: "test-api".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("rate limit exceeded".to_string()),
                timestamp: 0,
            });
            let failure_context = agent_session.oauth_runtime_context();
            agent_session
                .record_oauth_runtime_outcome(failure_context, &failure_result)
                .await;

            let fallback_token = agent_session
                .auth_storage
                .as_ref()
                .expect("auth storage")
                .resolve_api_key("openai", None)
                .expect("resolver fallback token");
            assert_ne!(
                fallback_token, selected_token,
                "resolver should avoid account under cooldown"
            );

            let persisted = std::fs::read_to_string(&auth_path).expect("read auth");
            let persisted_json: serde_json::Value =
                serde_json::from_str(&persisted).expect("parse auth json");
            let openai_accounts = persisted_json
                .get("oauth_pools")
                .and_then(|pools| pools.get("openai"))
                .and_then(|pool| pool.get("accounts"))
                .and_then(serde_json::Value::as_object)
                .expect("openai accounts");
            let failed_record = openai_accounts
                .values()
                .find(|record| {
                    record
                        .get("credential")
                        .and_then(|credential| credential.get("access_token"))
                        .and_then(serde_json::Value::as_str)
                        == Some(selected_token.as_str())
                })
                .expect("failed account record");
            assert!(
                failed_record
                    .get("health")
                    .and_then(|health| health.get("cooldown_until_ms"))
                    .and_then(serde_json::Value::as_i64)
                    .is_some(),
                "failed account should receive a cooldown marker"
            );

            agent_session.agent.stream_options_mut().api_key = Some(fallback_token.clone());
            let success_result: Result<AssistantMessage> = Ok(AssistantMessage {
                content: vec![],
                api: "test-api".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            });
            let success_context = agent_session.oauth_runtime_context();
            agent_session
                .record_oauth_runtime_outcome(success_context, &success_result)
                .await;

            let persisted_after_success =
                std::fs::read_to_string(&auth_path).expect("read auth after success");
            let persisted_success_json: serde_json::Value =
                serde_json::from_str(&persisted_after_success).expect("parse auth json");
            let success_record = persisted_success_json
                .get("oauth_pools")
                .and_then(|pools| pools.get("openai"))
                .and_then(|pool| pool.get("accounts"))
                .and_then(serde_json::Value::as_object)
                .and_then(|accounts| {
                    accounts.values().find(|record| {
                        record
                            .get("credential")
                            .and_then(|credential| credential.get("access_token"))
                            .and_then(serde_json::Value::as_str)
                            == Some(fallback_token.as_str())
                    })
                })
                .expect("success account record");
            assert!(
                success_record
                    .get("health")
                    .and_then(|health| health.get("last_success_ms"))
                    .and_then(serde_json::Value::as_i64)
                    .is_some(),
                "success account should record last_success_ms"
            );
            assert_eq!(
                success_record
                    .get("health")
                    .and_then(|health| health.get("requires_relogin"))
                    .and_then(serde_json::Value::as_bool),
                Some(false),
            );
        });
    }

    #[test]
    fn apply_session_model_selection_updates_stream_credentials_and_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "openai-key".to_string(),
            },
        );

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.apply_session_model_selection("openai", "gpt-4o");

        assert_eq!(agent_session.agent.provider().name(), "openai");
        assert_eq!(agent_session.agent.provider().model_id(), "gpt-4o");
        assert_eq!(
            agent_session.agent.stream_options().api_key.as_deref(),
            Some("openai-key")
        );
        assert!(
            agent_session.agent.stream_options().headers.is_empty(),
            "stream headers should be refreshed from selected model entry"
        );
    }

    #[test]
    fn apply_session_model_selection_clears_stale_key_when_target_has_no_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.apply_session_model_selection("openai", "gpt-4o");

        assert_eq!(agent_session.agent.provider().name(), "openai");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            None,
            "stale key must be cleared when target model has no configured key"
        );
    }

    #[test]
    fn apply_session_model_selection_treats_blank_model_key_as_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("load auth");

        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "blank-model".to_string(),
                name: "Blank Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: Some("   ".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }]);

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.set_model_registry(registry);
        agent_session.apply_session_model_selection("acme", "blank-model");

        assert_eq!(agent_session.agent.provider().name(), "acme");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            None,
            "blank model keys must not be treated as valid credentials"
        );
    }

    #[test]
    fn apply_session_model_selection_refreshes_key_when_model_is_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key-1".to_string(),
            },
        );

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.apply_session_model_selection("anthropic", "claude-sonnet-4-6");
        assert_eq!(
            agent_session.agent.stream_options().api_key.as_deref(),
            Some("anthropic-key-1"),
            "initial refresh should replace stale stream key even without provider/model change"
        );

        let mut updated_auth = auth.clone();
        updated_auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key-2".to_string(),
            },
        );
        agent_session.set_auth_storage(updated_auth);
        agent_session.apply_session_model_selection("anthropic", "claude-sonnet-4-6");

        assert_eq!(
            agent_session.agent.stream_options().api_key.as_deref(),
            Some("anthropic-key-2"),
            "stream key should refresh on repeated selection when auth changes"
        );
    }

    #[test]
    fn auto_compaction_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({
                "summary": "Compacted",
                "firstKeptEntryId": "abc123",
                "tokensBefore": 50000,
                "details": { "readFiles": [], "modifiedFiles": [] }
            })),
            aborted: false,
            will_retry: true,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json["result"].is_object());
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], true);
        assert!(json.get("errorMessage").is_none());
    }

    #[test]
    fn auto_compaction_end_with_error_serializes_error_message() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some("compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json["result"].is_null());
        assert_eq!(json["errorMessage"], "compaction failed");
    }

    #[test]
    fn auto_retry_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 4000,
            error_message: "rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 4000);
        assert_eq!(json["errorMessage"], "rate limited");
    }

    #[test]
    fn auto_retry_end_success_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_failure_serializes_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "max retries exceeded");
    }

    #[test]
    fn reliability_evidence_artifact_capture() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let artifact_store =
            Arc::new(FsArtifactStore::new(temp_dir.path()).expect("artifact store"));
        let mut capture = ReliabilityTraceCapture::new(
            "task-artifacts".to_string(),
            "capture verification evidence".to_string(),
            Some("provider/model".to_string()),
            Some(artifact_store.clone()),
        );

        capture.observe_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: json!({
                "command": "cargo test --lib session::tests::reliability_typed_entries_roundtrip -- --nocapture"
            }),
        });
        capture.observe_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("verification ok"))],
                details: Some(json!({ "exitCode": 0 })),
                is_error: false,
            },
            is_error: false,
        });

        let snapshot = capture.snapshot();
        assert_eq!(snapshot.evidence.len(), 1);
        let evidence = &snapshot.evidence[0];
        assert_eq!(evidence.task_id, "task-artifacts");
        assert_eq!(
            evidence.command,
            "cargo test --lib session::tests::reliability_typed_entries_roundtrip -- --nocapture"
        );
        assert_eq!(evidence.env_id.as_deref(), Some("provider/model"));
        assert_eq!(evidence.exit_code, 0);
        assert_eq!(evidence.status, crate::reliability::EvidenceStatus::Passed);
        assert_eq!(evidence.artifact_ids.len(), 1);

        let mut query = ArtifactQuery::new("task-artifacts");
        query.kind = Some("stdout".to_string());
        query.limit = 10;
        let ids = artifact_store.list(&query).expect("list artifacts");
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], evidence.artifact_ids[0]);

        let bytes = artifact_store.load(&ids[0]).expect("load artifact");
        assert_eq!(String::from_utf8_lossy(&bytes), "verification ok");
    }

    #[test]
    fn reliability_phase_gates_enforced() {
        let mut capture = ReliabilityTraceCapture::new(
            "task-phase-gates".to_string(),
            "enforce phase gates".to_string(),
            Some("provider/model".to_string()),
            None,
        );

        capture.observe_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-verify".to_string(),
            tool_name: "bash".to_string(),
            args: json!({ "command": "cargo test --lib agent::tests::reliability_phase_gates_enforced -- --nocapture" }),
        });
        capture.observe_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-verify".to_string(),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("verify output"))],
                details: Some(json!({ "exitCode": 0 })),
                is_error: false,
            },
            is_error: false,
        });
        let snapshot = capture.snapshot();
        assert!(
            snapshot
                .phase_violations
                .iter()
                .any(|msg| msg.contains("invalid phase")),
            "verify without execute must raise phase violation"
        );

        let stale_submit =
            capture.validate_submit_fence(&capture.lease_id, capture.fence_token + 1);
        assert!(stale_submit.is_err(), "stale fence token must be rejected");

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let make_agent_session = |mode| {
                let agent = Agent::new(
                    Arc::new(SilentProvider),
                    ToolRegistry::new(&[], Path::new("."), None),
                    AgentConfig {
                        system_prompt: None,
                        max_tool_iterations: 50,
                        stream_options: StreamOptions::default(),
                        block_images: false,
                    },
                );
                AgentSession::new(
                    agent,
                    Arc::new(Mutex::new(Session::in_memory())),
                    false,
                    ResolvedCompactionSettings::default(),
                )
                .with_reliability_enabled(true)
                .with_reliability_mode(mode)
            };

            let result: Result<AssistantMessage> = Ok(AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("ok"))],
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            });

            let hard_session = make_agent_session(ReliabilityEnforcementMode::Hard);
            let hard_outcome = hard_session
                .append_reliability_trace_entries(snapshot.clone(), &result)
                .await;
            assert!(
                hard_outcome.is_err(),
                "hard mode must reject phase violations"
            );

            let soft_session = make_agent_session(ReliabilityEnforcementMode::Soft);
            let soft_outcome = soft_session
                .append_reliability_trace_entries(snapshot, &result)
                .await;
            assert!(
                soft_outcome.is_ok(),
                "soft mode should record phase violations without rejecting"
            );
        });
    }
}
