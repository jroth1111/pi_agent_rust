use crate::agent::{AbortHandle, AgentSession};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::extensions::ExtensionManager;
use crate::model::ImageContent;
use crate::rpc::{
    RpcOptions, RpcSharedState, RpcUiBridgeState, RunningBash, StreamingBehavior,
    build_user_message, is_extension_command, parse_prompt_images, parse_streaming_behavior,
    rpc_emit_extension_ui_request, rpc_parse_extension_ui_response,
    rpc_parse_extension_ui_response_id, run_bash_rpc, run_prompt_with_retry,
};
use crate::surface::rpc_protocol::{response_error, response_error_with_hints, response_ok};
use asupersync::channel::oneshot;
use asupersync::sync::Mutex;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
