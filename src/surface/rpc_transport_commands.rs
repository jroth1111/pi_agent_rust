use crate::agent::{AbortHandle, AgentEvent, AgentSession};
use crate::agent_cx::AgentCx;
use crate::compaction::{
    ResolvedCompactionSettings, compact, compaction_details_to_value, prepare_compaction,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extensions::{ExtensionManager, ExtensionUiRequest, ExtensionUiResponse};
use crate::model::{
    ContentBlock, ImageContent, Message, StopReason, TextContent, UserContent, UserMessage,
};
use crate::rpc::{RpcOptions, RpcSharedState, RpcUiBridgeState, RunningBash, current_model_entry};
use crate::surface::rpc_protocol::{
    error_hints_value, response_error, response_error_with_hints, response_ok,
};
use crate::tools::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};
use asupersync::channel::oneshot;
use asupersync::runtime::RuntimeHandle;
use asupersync::sync::{Mutex, OwnedMutexGuard};
use asupersync::time::{sleep, wall_now};
use memchr::memchr_iter;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone)]
pub(crate) struct BashRpcResult {
    pub(crate) output: String,
    pub(crate) exit_code: i32,
    pub(crate) cancelled: bool,
    pub(crate) truncated: bool,
    pub(crate) full_output_path: Option<String>,
}

pub(crate) struct RpcTransportCommandContext<'a> {
    pub(crate) cx: &'a AgentCx,
    pub(crate) session: &'a Arc<Mutex<AgentSession>>,
    pub(crate) shared_state: &'a Arc<Mutex<RpcSharedState>>,
    pub(crate) options: &'a RpcOptions,
    pub(crate) out_tx: &'a std::sync::mpsc::Sender<String>,
    pub(crate) is_streaming: &'a Arc<AtomicBool>,
    pub(crate) is_compacting: &'a Arc<AtomicBool>,
    pub(crate) abort_handle: &'a Arc<Mutex<Option<AbortHandle>>>,
    pub(crate) bash_state: &'a Arc<Mutex<Option<RunningBash>>>,
    pub(crate) retry_abort: &'a Arc<AtomicBool>,
    pub(crate) rpc_extension_manager: Option<&'a ExtensionManager>,
    pub(crate) rpc_ui_state: Option<&'a Arc<Mutex<RpcUiBridgeState>>>,
}

pub(crate) async fn handle_transport_command(
    parsed: &Value,
    command_type: &str,
    id: Option<String>,
    ctx: RpcTransportCommandContext<'_>,
) -> Result<Option<String>> {
    let response = match command_type {
        "prompt" => handle_prompt(parsed, id, ctx).await?,
        "steer" | "follow_up" => handle_followup_family(parsed, command_type, id, ctx).await?,
        "abort" => {
            let handle = ctx
                .abort_handle
                .lock(ctx.cx)
                .await
                .map_err(|err| Error::session(format!("abort lock failed: {err}")))?
                .clone();
            if let Some(handle) = handle {
                handle.abort();
            }
            Some(response_ok(id, command_type, None))
        }
        "bash" => handle_bash(parsed, id, ctx).await?,
        "abort_bash" => {
            let mut running = ctx
                .bash_state
                .lock(ctx.cx)
                .await
                .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?;
            if let Some(running_bash) = running.take() {
                let _ = running_bash.abort_tx.send(ctx.cx, ());
            }
            Some(response_ok(id, command_type, None))
        }
        "extension_ui_response" => handle_extension_ui_response(parsed, id, ctx).await?,
        _ => None,
    };

    Ok(response)
}

async fn handle_prompt(
    parsed: &Value,
    id: Option<String>,
    ctx: RpcTransportCommandContext<'_>,
) -> Result<Option<String>> {
    let Some(message) = parsed
        .get("message")
        .and_then(Value::as_str)
        .map(String::from)
    else {
        return Ok(Some(response_error(
            id,
            "prompt",
            "Missing message".to_string(),
        )));
    };

    let images = match parse_prompt_images(parsed.get("images")) {
        Ok(images) => images,
        Err(err) => return Ok(Some(response_error_with_hints(id, "prompt", &err))),
    };

    let streaming_behavior = match parse_streaming_behavior(parsed.get("streamingBehavior")) {
        Ok(value) => value,
        Err(err) => return Ok(Some(response_error_with_hints(id, "prompt", &err))),
    };

    let expanded = ctx.options.resources.expand_input(&message);

    if ctx.is_streaming.load(Ordering::SeqCst) {
        if streaming_behavior.is_none() {
            return Ok(Some(response_error(
                id,
                "prompt",
                "Agent is currently streaming; specify streamingBehavior".to_string(),
            )));
        }

        let queued_result = {
            let mut state = ctx
                .shared_state
                .lock(ctx.cx)
                .await
                .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
            match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    state.push_steering(build_user_message(&expanded, &images))
                }
                Some(StreamingBehavior::FollowUp) => {
                    state.push_follow_up(build_user_message(&expanded, &images))
                }
                None => Ok(()),
            }
        };

        return Ok(Some(match queued_result {
            Ok(()) => response_ok(id, "prompt", None),
            Err(err) => response_error_with_hints(id, "prompt", &err),
        }));
    }

    spawn_prompt_run(ctx, expanded, images);
    Ok(Some(response_ok(id, "prompt", None)))
}

async fn handle_followup_family(
    parsed: &Value,
    command_type: &str,
    id: Option<String>,
    ctx: RpcTransportCommandContext<'_>,
) -> Result<Option<String>> {
    let Some(message) = parsed
        .get("message")
        .and_then(Value::as_str)
        .map(String::from)
    else {
        return Ok(Some(response_error(
            id,
            command_type,
            "Missing message".to_string(),
        )));
    };

    let expanded = ctx.options.resources.expand_input(&message);
    if is_extension_command(&message, &expanded) {
        return Ok(Some(response_error(
            id,
            command_type,
            format!("Extension commands are not allowed with {command_type}"),
        )));
    }

    if ctx.is_streaming.load(Ordering::SeqCst) {
        let result = {
            let mut state = ctx
                .shared_state
                .lock(ctx.cx)
                .await
                .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
            if command_type == "steer" {
                state.push_steering(build_user_message(&expanded, &[]))
            } else {
                state.push_follow_up(build_user_message(&expanded, &[]))
            }
        };

        return Ok(Some(match result {
            Ok(()) => response_ok(id, command_type, None),
            Err(err) => response_error_with_hints(id, command_type, &err),
        }));
    }

    spawn_prompt_run(ctx, expanded, Vec::new());
    Ok(Some(response_ok(id, command_type, None)))
}

async fn handle_bash(
    parsed: &Value,
    id: Option<String>,
    ctx: RpcTransportCommandContext<'_>,
) -> Result<Option<String>> {
    let Some(command) = parsed.get("command").and_then(Value::as_str) else {
        return Ok(Some(response_error(
            id,
            "bash",
            "Missing command".to_string(),
        )));
    };

    let mut running = ctx
        .bash_state
        .lock(ctx.cx)
        .await
        .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?;
    if running.is_some() {
        return Ok(Some(response_error(
            id,
            "bash",
            "Bash command already running".to_string(),
        )));
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    let (abort_tx, abort_rx) = oneshot::channel();
    *running = Some(RunningBash {
        id: run_id.clone(),
        abort_tx,
    });
    drop(running);

    let out_tx = ctx.out_tx.clone();
    let session = Arc::clone(ctx.session);
    let bash_state = Arc::clone(ctx.bash_state);
    let command = command.to_string();
    let id_clone = id.clone();
    let runtime_handle = ctx.options.runtime_handle.clone();

    runtime_handle.spawn(async move {
        let cx = AgentCx::for_request();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let result = run_bash_rpc(&cwd, &command, abort_rx).await;

        let response = match result {
            Ok(result) => {
                if let Ok(mut guard) = session.lock(&cx).await {
                    if let Ok(mut inner_session) = guard.session.lock(&cx).await {
                        inner_session.append_message(
                            crate::session::SessionMessage::BashExecution {
                                command: command.clone(),
                                output: result.output.clone(),
                                exit_code: result.exit_code,
                                cancelled: Some(result.cancelled),
                                truncated: Some(result.truncated),
                                full_output_path: result.full_output_path.clone(),
                                timestamp: Some(chrono::Utc::now().timestamp_millis()),
                                extra: std::collections::HashMap::default(),
                            },
                        );
                    }
                    let _ = guard.persist_session().await;
                }

                response_ok(
                    id_clone,
                    "bash",
                    Some(json!({
                        "output": result.output,
                        "exitCode": result.exit_code,
                        "cancelled": result.cancelled,
                        "truncated": result.truncated,
                        "fullOutputPath": result.full_output_path,
                    })),
                )
            }
            Err(err) => response_error_with_hints(id_clone, "bash", &err),
        };

        let _ = out_tx.send(response);
        if let Ok(mut running) = bash_state.lock(&cx).await {
            if running.as_ref().is_some_and(|r| r.id == run_id) {
                *running = None;
            }
        }
    });

    Ok(None)
}

async fn handle_extension_ui_response(
    parsed: &Value,
    id: Option<String>,
    ctx: RpcTransportCommandContext<'_>,
) -> Result<Option<String>> {
    if let (Some(manager), Some(ui_state)) = (ctx.rpc_extension_manager, ctx.rpc_ui_state) {
        let Some(request_id) = rpc_parse_extension_ui_response_id(parsed) else {
            return Ok(Some(response_error(
                id,
                "extension_ui_response",
                "Missing requestId (or id) field",
            )));
        };

        let (response, next_request) = {
            let Ok(mut guard) = ui_state.lock(ctx.cx).await else {
                return Ok(Some(response_error(
                    id,
                    "extension_ui_response",
                    "Extension UI bridge unavailable",
                )));
            };

            let Some(active) = guard.active.clone() else {
                return Ok(Some(response_error(
                    id,
                    "extension_ui_response",
                    "No active extension UI request",
                )));
            };

            if active.id != request_id {
                return Ok(Some(response_error(
                    id,
                    "extension_ui_response",
                    format!("Unexpected requestId: {request_id} (active: {})", active.id),
                )));
            }

            let response = match rpc_parse_extension_ui_response(parsed, &active) {
                Ok(response) => response,
                Err(message) => {
                    return Ok(Some(response_error(id, "extension_ui_response", message)));
                }
            };

            guard.active = None;
            let next = guard.queue.pop_front();
            if let Some(ref next) = next {
                guard.active = Some(next.clone());
            }
            (response, next)
        };

        let resolved = manager.respond_ui(response);
        if let Some(next) = next_request {
            rpc_emit_extension_ui_request(
                &ctx.options.runtime_handle,
                Arc::clone(ui_state),
                manager.clone(),
                ctx.out_tx.clone(),
                next,
            );
        }
        Ok(Some(response_ok(
            id,
            "extension_ui_response",
            Some(json!({ "resolved": resolved })),
        )))
    } else {
        Ok(Some(response_ok(id, "extension_ui_response", None)))
    }
}

fn spawn_prompt_run(
    ctx: RpcTransportCommandContext<'_>,
    expanded: String,
    images: Vec<ImageContent>,
) {
    ctx.is_streaming.store(true, Ordering::SeqCst);

    let out_tx = ctx.out_tx.clone();
    let session = Arc::clone(ctx.session);
    let shared_state = Arc::clone(ctx.shared_state);
    let is_streaming = Arc::clone(ctx.is_streaming);
    let is_compacting = Arc::clone(ctx.is_compacting);
    let abort_handle_slot = Arc::clone(ctx.abort_handle);
    let retry_abort = Arc::clone(ctx.retry_abort);
    let options = ctx.options.clone();
    let runtime_handle = options.runtime_handle.clone();
    runtime_handle.spawn(async move {
        let cx = AgentCx::for_request();
        run_prompt_with_retry(
            session,
            shared_state,
            is_streaming,
            is_compacting,
            abort_handle_slot,
            out_tx,
            retry_abort,
            options,
            expanded,
            images,
            cx,
        )
        .await;
    });
}

pub(crate) fn build_user_message(text: &str, images: &[ImageContent]) -> Message {
    let timestamp = chrono::Utc::now().timestamp_millis();
    if images.is_empty() {
        return Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp,
        });
    }
    let mut blocks = vec![ContentBlock::Text(TextContent::new(text.to_string()))];
    for image in images {
        blocks.push(ContentBlock::Image(image.clone()));
    }
    Message::User(UserMessage {
        content: UserContent::Blocks(blocks),
        timestamp,
    })
}

pub(crate) fn is_extension_command(message: &str, expanded: &str) -> bool {
    // Extension commands start with `/` but are not expanded by the resource loader
    // (skills and prompt templates are expanded before queueing/sending).
    message.trim_start().starts_with('/') && message == expanded
}

pub(crate) fn parse_streaming_behavior(value: Option<&Value>) -> Result<Option<StreamingBehavior>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(s) = value.as_str() else {
        return Err(Error::validation("streamingBehavior must be a string"));
    };
    match s {
        "steer" => Ok(Some(StreamingBehavior::Steer)),
        "follow-up" | "followUp" => Ok(Some(StreamingBehavior::FollowUp)),
        _ => Err(Error::validation(format!("Invalid streamingBehavior: {s}"))),
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_prompt_with_retry(
    session: Arc<Mutex<AgentSession>>,
    shared_state: Arc<Mutex<RpcSharedState>>,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    abort_handle_slot: Arc<Mutex<Option<AbortHandle>>>,
    out_tx: std::sync::mpsc::Sender<String>,
    retry_abort: Arc<AtomicBool>,
    options: RpcOptions,
    message: String,
    images: Vec<ImageContent>,
    cx: AgentCx,
) {
    retry_abort.store(false, Ordering::SeqCst);
    is_streaming.store(true, Ordering::SeqCst);

    let max_retries = options.config.retry_max_retries();
    let mut retry_count: u32 = 0;
    let mut success = false;
    let mut final_error: Option<String> = None;
    let mut final_error_hints: Option<Value> = None;

    loop {
        let (abort_handle, abort_signal) = AbortHandle::new();
        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
            *guard = Some(abort_handle);
        } else {
            is_streaming.store(false, Ordering::SeqCst);
            return;
        }

        let runtime_for_events = options.runtime_handle.clone();

        let result = {
            let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    final_error = Some(format!("session lock failed: {err}"));
                    final_error_hints = None;
                    break;
                }
            };
            let extensions = guard.extensions.as_ref().map(|r| r.manager().clone());
            let runtime_for_events_handler = runtime_for_events.clone();
            let event_tx = out_tx.clone();
            let coalescer = extensions
                .as_ref()
                .map(|m| crate::extensions::EventCoalescer::new(m.clone()));
            let event_handler = move |event: AgentEvent| {
                let serialized = if let AgentEvent::AgentEnd {
                    messages, error, ..
                } = &event
                {
                    json!({
                        "type": "agent_end",
                        "messages": messages,
                        "error": error,
                    })
                    .to_string()
                } else {
                    serde_json::to_string(&event).unwrap_or_else(|err| {
                        json!({
                            "type": "event_serialize_error",
                            "error": err.to_string(),
                        })
                        .to_string()
                    })
                };
                let _ = event_tx.send(serialized);
                if let Some(coal) = &coalescer {
                    coal.dispatch_agent_event_lazy(&event, &runtime_for_events_handler);
                }
            };

            if images.is_empty() {
                guard
                    .run_text_with_abort(message.clone(), Some(abort_signal), event_handler)
                    .await
            } else {
                let mut blocks = vec![ContentBlock::Text(TextContent::new(message.clone()))];
                for image in &images {
                    blocks.push(ContentBlock::Image(image.clone()));
                }
                guard
                    .run_with_content_with_abort(blocks, Some(abort_signal), event_handler)
                    .await
            }
        };

        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
            *guard = None;
        }

        match result {
            Ok(message) => {
                if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                    final_error = message
                        .error_message
                        .clone()
                        .or_else(|| Some("Request error".to_string()));
                    final_error_hints = None;
                    if message.stop_reason == StopReason::Aborted {
                        break;
                    }
                    if let Some(ref err_msg) = final_error {
                        let context_window = if let Ok(guard) =
                            OwnedMutexGuard::lock(Arc::clone(&session), &cx).await
                        {
                            guard.session.lock(&cx).await.map_or(None, |inner| {
                                current_model_entry(&inner, &options)
                                    .map(|e| e.model.context_window)
                            })
                        } else {
                            None
                        };
                        if !crate::error::is_retryable_error(
                            err_msg,
                            Some(message.usage.input),
                            context_window,
                        ) {
                            break;
                        }
                    }
                } else {
                    success = true;
                    break;
                }
            }
            Err(err) => {
                let err_str = err.to_string();
                if !crate::error::is_retryable_error(&err_str, None, None) {
                    final_error = Some(err_str);
                    final_error_hints = Some(error_hints_value(&err));
                    break;
                }
                final_error = Some(err_str);
                final_error_hints = Some(error_hints_value(&err));
            }
        }

        let retry_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
            .await
            .is_ok_and(|state| state.auto_retry_enabled());
        if !retry_enabled || retry_count >= max_retries {
            break;
        }

        retry_count += 1;
        let delay_ms = retry_delay_ms(&options.config, retry_count);
        let error_message = final_error
            .clone()
            .unwrap_or_else(|| "Request error".to_string());
        let _ = out_tx.send(event(&json!({
            "type": "auto_retry_start",
            "attempt": retry_count,
            "maxAttempts": max_retries,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        })));

        let delay = Duration::from_millis(delay_ms as u64);
        let start = std::time::Instant::now();
        while start.elapsed() < delay {
            if retry_abort.load(Ordering::SeqCst) {
                break;
            }
            sleep(wall_now(), Duration::from_millis(50)).await;
        }

        if retry_abort.load(Ordering::SeqCst) {
            final_error = Some("Retry aborted".to_string());
            break;
        }
    }

    if retry_count > 0 {
        let _ = out_tx.send(event(&json!({
            "type": "auto_retry_end",
            "success": success,
            "attempt": retry_count,
            "finalError": if success { Value::Null } else { json!(final_error.clone()) },
        })));
    }

    is_streaming.store(false, Ordering::SeqCst);

    if !success {
        if let Some(err) = final_error {
            let mut payload = json!({
                "type": "agent_end",
                "messages": [],
                "error": err
            });
            if let Some(hints) = final_error_hints {
                payload["errorHints"] = hints;
            }
            let _ = out_tx.send(event(&payload));
        }
        return;
    }

    let auto_compaction_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
        .await
        .is_ok_and(|state| state.auto_compaction_enabled());
    if auto_compaction_enabled {
        maybe_auto_compact(session, options, is_compacting, out_tx).await;
    }
}

pub(crate) fn event(value: &Value) -> String {
    value.to_string()
}

pub(crate) fn rpc_emit_extension_ui_request(
    runtime_handle: &RuntimeHandle,
    ui_state: Arc<Mutex<RpcUiBridgeState>>,
    manager: ExtensionManager,
    out_tx_ui: std::sync::mpsc::Sender<String>,
    request: ExtensionUiRequest,
) {
    let rpc_event = request.to_rpc_event();
    let _ = out_tx_ui.send(event(&rpc_event));

    if !request.expects_response() {
        return;
    }

    let Some(timeout_ms) = request.effective_timeout_ms() else {
        return;
    };

    let fire_ms = timeout_ms.saturating_sub(10).max(1);
    let request_id = request.id;
    let ui_state_timeout = Arc::clone(&ui_state);
    let manager_timeout = manager;
    let out_tx_timeout = out_tx_ui;
    let runtime_handle_inner = runtime_handle.clone();

    runtime_handle.spawn(async move {
        sleep(wall_now(), Duration::from_millis(fire_ms)).await;
        let cx = AgentCx::for_request();

        let next = {
            let Ok(mut guard) = ui_state_timeout.lock(cx.cx()).await else {
                return;
            };

            let Some(active) = guard.active.as_ref() else {
                return;
            };

            if active.id != request_id {
                return;
            }

            let _ = manager_timeout.respond_ui(ExtensionUiResponse {
                id: request_id,
                value: None,
                cancelled: true,
            });

            guard.active = None;
            let next = guard.queue.pop_front();
            if let Some(ref next) = next {
                guard.active = Some(next.clone());
            }
            next
        };

        if let Some(next) = next {
            rpc_emit_extension_ui_request(
                &runtime_handle_inner,
                ui_state_timeout,
                manager_timeout,
                out_tx_timeout,
                next,
            );
        }
    });
}

pub(crate) fn rpc_parse_extension_ui_response_id(parsed: &Value) -> Option<String> {
    let request_id = parsed
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);

    request_id.or_else(|| {
        parsed
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

pub(crate) fn rpc_parse_extension_ui_response(
    parsed: &Value,
    active: &ExtensionUiRequest,
) -> std::result::Result<ExtensionUiResponse, String> {
    let cancelled = parsed
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if cancelled {
        return Ok(ExtensionUiResponse {
            id: active.id.clone(),
            value: None,
            cancelled: true,
        });
    }

    match active.method.as_str() {
        "confirm" => {
            let value = parsed
                .get("confirmed")
                .and_then(Value::as_bool)
                .or_else(|| parsed.get("value").and_then(Value::as_bool))
                .ok_or_else(|| "confirm requires boolean `confirmed` (or `value`)".to_string())?;
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(Value::Bool(value)),
                cancelled: false,
            })
        }
        "select" => {
            let value = parsed
                .get("selected")
                .or_else(|| parsed.get("value"))
                .ok_or_else(|| "select requires `selected` (or `value`)".to_string())?;
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(value.clone()),
                cancelled: false,
            })
        }
        "input" | "editor" => {
            let value = parsed
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{} requires string `value`", active.method))?;
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(Value::String(value.to_string())),
                cancelled: false,
            })
        }
        "notify" => Ok(ExtensionUiResponse {
            id: active.id.clone(),
            value: None,
            cancelled: false,
        }),
        other => Err(format!("unsupported extension UI method: {other}")),
    }
}

pub(crate) fn retry_delay_ms(config: &Config, attempt: u32) -> u32 {
    let base = u64::from(config.retry_base_delay_ms());
    let max = u64::from(config.retry_max_delay_ms());
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
}

pub(crate) fn should_auto_compact(
    tokens_before: u64,
    context_window: u32,
    reserve_tokens: u32,
) -> bool {
    let reserve = u64::from(reserve_tokens);
    let window = u64::from(context_window);
    tokens_before > window.saturating_sub(reserve)
}

#[allow(clippy::too_many_lines)]
async fn maybe_auto_compact(
    session: Arc<Mutex<AgentSession>>,
    options: RpcOptions,
    is_compacting: Arc<AtomicBool>,
    out_tx: std::sync::mpsc::Sender<String>,
) {
    let cx = AgentCx::for_request();
    let (path_entries, context_window, reserve_tokens, settings) = {
        let Ok(guard) = session.lock(cx.cx()).await else {
            return;
        };
        let (path_entries, context_window) = {
            let Ok(mut inner_session) = guard.session.lock(cx.cx()).await else {
                return;
            };
            inner_session.ensure_entry_ids();
            let Some(entry) = current_model_entry(&inner_session, &options) else {
                return;
            };
            let path_entries = inner_session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            (path_entries, entry.model.context_window)
        };

        let reserve_tokens = options.config.compaction_reserve_tokens();
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens,
            keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
            ..Default::default()
        };

        (path_entries, context_window, reserve_tokens, settings)
    };

    let Some(prep) = prepare_compaction(&path_entries, settings) else {
        return;
    };
    if !should_auto_compact(prep.tokens_before, context_window, reserve_tokens) {
        return;
    }

    let _ = out_tx.send(event(&json!({
        "type": "auto_compaction_start",
        "reason": "threshold",
    })));
    is_compacting.store(true, Ordering::SeqCst);

    let (provider, key) = {
        let Ok(guard) = session.lock(cx.cx()).await else {
            is_compacting.store(false, Ordering::SeqCst);
            return;
        };
        let Some(key) = guard.agent.stream_options().api_key.clone() else {
            is_compacting.store(false, Ordering::SeqCst);
            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": Value::Null,
                "aborted": false,
                "willRetry": false,
                "errorMessage": "Missing API key for compaction",
            })));
            return;
        };
        (guard.agent.provider(), key)
    };

    let result = compact(prep, provider, &key, None).await;
    is_compacting.store(false, Ordering::SeqCst);

    match result {
        Ok(result) => {
            let details_value = match compaction_details_to_value(&result.details) {
                Ok(value) => value,
                Err(err) => {
                    let _ = out_tx.send(event(&json!({
                        "type": "auto_compaction_end",
                        "result": Value::Null,
                        "aborted": false,
                        "willRetry": false,
                        "errorMessage": err.to_string(),
                    })));
                    return;
                }
            };

            let Ok(mut guard) = session.lock(cx.cx()).await else {
                return;
            };
            let messages = {
                let Ok(mut inner_session) = guard.session.lock(cx.cx()).await else {
                    return;
                };
                inner_session.append_compaction(
                    result.summary.clone(),
                    result.first_kept_entry_id.clone(),
                    result.tokens_before,
                    Some(details_value.clone()),
                    None,
                );
                inner_session.to_messages_for_current_path()
            };
            let _ = guard.persist_session().await;
            guard.agent.replace_messages(messages);
            drop(guard);

            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": {
                    "summary": result.summary,
                    "firstKeptEntryId": result.first_kept_entry_id,
                    "tokensBefore": result.tokens_before,
                    "details": details_value,
                },
                "aborted": false,
                "willRetry": false,
            })));
        }
        Err(err) => {
            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": Value::Null,
                "aborted": false,
                "willRetry": false,
                "errorMessage": err.to_string(),
            })));
        }
    }
}

pub(crate) const fn line_count_from_newline_count(
    total_bytes: usize,
    newline_count: usize,
    last_byte_was_newline: bool,
) -> usize {
    if total_bytes == 0 {
        0
    } else if last_byte_was_newline {
        newline_count
    } else {
        newline_count.saturating_add(1)
    }
}

async fn ingest_bash_rpc_chunk(
    bytes: Vec<u8>,
    chunks: &mut VecDeque<Vec<u8>>,
    chunks_bytes: &mut usize,
    total_bytes: &mut usize,
    total_lines: &mut usize,
    last_byte_was_newline: &mut bool,
    temp_file: &mut Option<asupersync::fs::File>,
    temp_file_path: &mut Option<PathBuf>,
    spill_failed: &mut bool,
    max_chunks_bytes: usize,
) {
    if bytes.is_empty() {
        return;
    }

    *last_byte_was_newline = bytes.last().is_some_and(|byte| *byte == b'\n');
    *total_bytes = total_bytes.saturating_add(bytes.len());
    *total_lines = total_lines.saturating_add(memchr_iter(b'\n', &bytes).count());

    if *total_bytes > DEFAULT_MAX_BYTES && temp_file.is_none() && !*spill_failed {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("pi-rpc-bash-{id}.log"));

        let created = {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path).is_ok()
        };

        if created {
            match asupersync::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await
            {
                Ok(mut file) => {
                    for existing in chunks.iter() {
                        use asupersync::io::AsyncWriteExt;
                        if let Err(e) = file.write_all(existing).await {
                            tracing::warn!("Failed to flush bash chunk to temp file: {e}");
                            *spill_failed = true;
                            break;
                        }
                    }
                    if !*spill_failed {
                        *temp_file = Some(file);
                        *temp_file_path = Some(path);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to reopen bash temp file async: {e}");
                    let _ = std::fs::remove_file(&path);
                    *spill_failed = true;
                }
            }
        } else {
            *spill_failed = true;
        }
    }

    if let Some(file) = temp_file.as_mut() {
        if *total_bytes <= crate::tools::BASH_FILE_LIMIT_BYTES {
            use asupersync::io::AsyncWriteExt;
            if let Err(e) = file.write_all(&bytes).await {
                tracing::warn!("Failed to write bash chunk to temp file: {e}");
                *spill_failed = true;
                *temp_file = None;
            }
        } else if !*spill_failed {
            tracing::warn!("Bash output exceeded hard limit; stopping file log");
            *spill_failed = true;
            *temp_file = None;
        }
    }

    *chunks_bytes = chunks_bytes.saturating_add(bytes.len());
    chunks.push_back(bytes);
    while *chunks_bytes > max_chunks_bytes && chunks.len() > 1 {
        if let Some(front) = chunks.pop_front() {
            *chunks_bytes = chunks_bytes.saturating_sub(front.len());
        }
    }
}

pub(crate) async fn run_bash_rpc(
    cwd: &Path,
    command: &str,
    abort_rx: oneshot::Receiver<()>,
) -> Result<BashRpcResult> {
    #[derive(Clone, Copy)]
    enum StreamKind {
        Stdout,
        Stderr,
    }

    struct StreamChunk {
        kind: StreamKind,
        bytes: Vec<u8>,
    }

    fn pump_stream(
        mut reader: impl std::io::Read,
        tx: std::sync::mpsc::SyncSender<StreamChunk>,
        kind: StreamKind,
    ) {
        let mut buf = [0u8; 8192];
        loop {
            let read = match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let chunk = StreamChunk {
                kind,
                bytes: buf[..read].to_vec(),
            };
            if tx.send(chunk).is_err() {
                break;
            }
        }
    }

    let shell = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("sh");

    let command = format!("trap 'code=$?; wait; exit $code' EXIT\n{command}");

    let mut child = std::process::Command::new(shell)
        .arg("-c")
        .arg(&command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell: {e}")))?;

    let Some(stdout) = child.stdout.take() else {
        return Err(Error::tool("bash", "Missing stdout".to_string()));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(Error::tool("bash", "Missing stderr".to_string()));
    };

    let mut guard = crate::tools::ProcessGuard::new(child, true);

    let (tx, rx) = std::sync::mpsc::sync_channel::<StreamChunk>(128);
    let tx_stdout = tx.clone();
    let stdout_handle =
        std::thread::spawn(move || pump_stream(stdout, tx_stdout, StreamKind::Stdout));
    let stderr_handle = std::thread::spawn(move || pump_stream(stderr, tx, StreamKind::Stderr));

    let tick = Duration::from_millis(10);
    let mut chunks: VecDeque<Vec<u8>> = VecDeque::new();
    let mut chunks_bytes = 0usize;
    let mut total_bytes = 0usize;
    let mut total_lines = 0usize;
    let mut last_byte_was_newline = false;
    let mut temp_file: Option<asupersync::fs::File> = None;
    let mut temp_file_path: Option<PathBuf> = None;
    let max_chunks_bytes = DEFAULT_MAX_BYTES * 2;

    let mut cancelled = false;
    let mut spill_failed = false;

    let exit_code = loop {
        while let Ok(chunk) = rx.try_recv() {
            let _stream_kind = chunk.kind;
            ingest_bash_rpc_chunk(
                chunk.bytes,
                &mut chunks,
                &mut chunks_bytes,
                &mut total_bytes,
                &mut total_lines,
                &mut last_byte_was_newline,
                &mut temp_file,
                &mut temp_file_path,
                &mut spill_failed,
                max_chunks_bytes,
            )
            .await;
        }

        if !cancelled && abort_rx.try_recv().is_ok() {
            cancelled = true;
            if let Ok(Some(status)) = guard.kill() {
                break status.code().unwrap_or(-1);
            }
        }

        match guard.try_wait_child() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(err) => {
                return Err(Error::tool(
                    "bash",
                    format!("Failed to wait for process: {err}"),
                ));
            }
        }

        sleep(wall_now(), tick).await;
    };

    let drain_deadline = Instant::now() + Duration::from_secs(2);
    let mut drain_timed_out = false;
    loop {
        match rx.try_recv() {
            Ok(chunk) => {
                let _stream_kind = chunk.kind;
                ingest_bash_rpc_chunk(
                    chunk.bytes,
                    &mut chunks,
                    &mut chunks_bytes,
                    &mut total_bytes,
                    &mut total_lines,
                    &mut last_byte_was_newline,
                    &mut temp_file,
                    &mut temp_file_path,
                    &mut spill_failed,
                    max_chunks_bytes,
                )
                .await;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if Instant::now() >= drain_deadline {
                    drain_timed_out = true;
                    break;
                }
                sleep(wall_now(), tick).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    drop(rx);

    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    drop(temp_file);

    let mut combined = Vec::with_capacity(chunks_bytes);
    for chunk in chunks {
        combined.extend_from_slice(&chunk);
    }
    let tail_output = String::from_utf8_lossy(&combined).to_string();

    let mut truncation = truncate_tail(tail_output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if total_bytes > chunks_bytes {
        truncation.truncated = true;
        truncation.truncated_by = Some(crate::tools::TruncatedBy::Bytes);
        truncation.total_bytes = total_bytes;
        truncation.total_lines =
            line_count_from_newline_count(total_bytes, total_lines, last_byte_was_newline);
    } else if drain_timed_out {
        truncation.truncated = true;
        truncation.truncated_by = Some(crate::tools::TruncatedBy::Bytes);
    }
    let will_truncate = truncation.truncated;

    let mut output_text = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content
    };

    if drain_timed_out {
        output_text.push_str("\n... [Output truncated: drain timeout]");
    }

    Ok(BashRpcResult {
        output: output_text,
        exit_code,
        cancelled,
        truncated: will_truncate,
        full_output_path: temp_file_path.map(|p| p.display().to_string()),
    })
}

pub(crate) fn parse_prompt_images(value: Option<&Value>) -> Result<Vec<ImageContent>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(arr) = value.as_array() else {
        return Err(Error::validation("images must be an array"));
    };

    let mut images = Vec::new();
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type != "image" {
            continue;
        }
        let Some(source) = obj.get("source").and_then(Value::as_object) else {
            continue;
        };
        let source_type = source.get("type").and_then(Value::as_str).unwrap_or("");
        if source_type != "base64" {
            continue;
        }
        let Some(media_type) = source.get("mediaType").and_then(Value::as_str) else {
            continue;
        };
        let Some(data) = source.get("data").and_then(Value::as_str) else {
            continue;
        };
        images.push(ImageContent {
            data: data.to_string(),
            mime_type: media_type.to_string(),
        });
    }
    Ok(images)
}
