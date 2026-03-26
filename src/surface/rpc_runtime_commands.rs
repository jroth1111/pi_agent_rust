use crate::agent::AgentSession;
use crate::agent_cx::AgentCx;
use crate::config::parse_queue_mode;
use crate::error::{Error, Result};
use crate::model::ThinkingLevel;
use crate::providers;
use crate::rpc::{RpcOptions, provider_ids_match};
use crate::session::SessionMessage;
use crate::surface::rpc_protocol::{response_error, response_error_with_hints, response_ok};
use crate::surface::rpc_support::{
    RpcSharedState, RpcStateSnapshot, apply_model_change, apply_thinking_level,
    available_thinking_levels, current_model_entry, cycle_model_for_rpc, export_html_snapshot,
    last_assistant_text, model_requires_configured_credential, resolve_model_key,
    rpc_model_from_entry, rpc_session_message_value, session_state, session_stats,
    sync_agent_queue_modes,
};
use asupersync::sync::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct RpcRuntimeCommandContext<'a> {
    pub(crate) cx: &'a AgentCx,
    pub(crate) session: &'a Arc<Mutex<AgentSession>>,
    pub(crate) session_handle: &'a Arc<Mutex<crate::session::Session>>,
    pub(crate) shared_state: &'a Arc<Mutex<RpcSharedState>>,
    pub(crate) options: &'a RpcOptions,
    pub(crate) is_streaming: &'a Arc<AtomicBool>,
    pub(crate) is_compacting: &'a Arc<AtomicBool>,
    pub(crate) retry_abort: &'a Arc<AtomicBool>,
}

pub(crate) async fn handle_runtime_command(
    parsed: &Value,
    command_type: &str,
    id: Option<String>,
    ctx: RpcRuntimeCommandContext<'_>,
) -> Result<Option<String>> {
    let response =
        match command_type {
            "get_state" => {
                let snapshot = {
                    let state = ctx
                        .shared_state
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    RpcStateSnapshot::from(&*state)
                };
                let data = {
                    let inner_session = ctx.session_handle.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    session_state(
                        &inner_session,
                        ctx.options,
                        &snapshot,
                        ctx.is_streaming.load(Ordering::SeqCst),
                        ctx.is_compacting.load(Ordering::SeqCst),
                    )
                };
                Some(response_ok(id, command_type, Some(data)))
            }
            "get_session_stats" => {
                let data = {
                    let inner_session = ctx.session_handle.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    session_stats(&inner_session)
                };
                Some(response_ok(id, command_type, Some(data)))
            }
            "get_messages" => {
                let messages = {
                    let inner_session = ctx.session_handle.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner_session
                        .entries_for_current_path()
                        .iter()
                        .filter_map(|entry| match entry {
                            crate::session::SessionEntry::Message(msg) => match msg.message {
                                SessionMessage::User { .. }
                                | SessionMessage::Assistant { .. }
                                | SessionMessage::ToolResult { .. }
                                | SessionMessage::BashExecution { .. } => Some(msg.message.clone()),
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                }
                .into_iter()
                .map(rpc_session_message_value)
                .collect::<Vec<_>>();
                Some(response_ok(
                    id,
                    command_type,
                    Some(json!({ "messages": messages })),
                ))
            }
            "get_available_models" => {
                let models = ctx
                    .options
                    .available_models
                    .iter()
                    .map(rpc_model_from_entry)
                    .collect::<Vec<_>>();
                Some(response_ok(
                    id,
                    command_type,
                    Some(json!({ "models": models })),
                ))
            }
            "set_model" => {
                let Some(provider) = parsed.get("provider").and_then(Value::as_str) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing provider".to_string(),
                    )));
                };
                let Some(model_id) = parsed.get("modelId").and_then(Value::as_str) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing modelId".to_string(),
                    )));
                };

                let Some(entry) = ctx
                    .options
                    .available_models
                    .iter()
                    .find(|m| {
                        provider_ids_match(&m.model.provider, provider)
                            && m.model.id.eq_ignore_ascii_case(model_id)
                    })
                    .cloned()
                else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        format!("Model not found: {provider}/{model_id}"),
                    )));
                };

                let key = resolve_model_key(&ctx.options.auth, &entry);
                if model_requires_configured_credential(&entry) && key.is_none() {
                    let err = Error::auth(format!(
                        "Missing credentials for {}/{}",
                        entry.model.provider, entry.model.id
                    ));
                    return Ok(Some(response_error_with_hints(id, command_type, &err)));
                }

                {
                    let mut guard = ctx
                        .session
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let provider_impl = providers::create_provider(
                        &entry,
                        guard
                            .extensions
                            .as_ref()
                            .map(crate::extensions::ExtensionRegion::manager),
                    )?;
                    guard.agent.set_provider(provider_impl);
                    guard.agent.stream_options_mut().api_key.clone_from(&key);
                    guard
                        .agent
                        .stream_options_mut()
                        .headers
                        .clone_from(&entry.headers);

                    apply_model_change(&mut guard, &entry).await?;

                    let current_thinking = guard
                        .agent
                        .stream_options()
                        .thinking_level
                        .unwrap_or_default();
                    let clamped = entry.clamp_thinking_level(current_thinking);
                    if clamped != current_thinking {
                        apply_thinking_level(&mut guard, clamped).await?;
                    }
                }

                Some(response_ok(
                    id,
                    command_type,
                    Some(rpc_model_from_entry(&entry)),
                ))
            }
            "cycle_model" => {
                let (entry, thinking_level, is_scoped) = {
                    let mut guard = ctx
                        .session
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let Some(result) = cycle_model_for_rpc(&mut guard, ctx.options).await? else {
                        return Ok(Some(response_ok(id, command_type, Some(Value::Null))));
                    };
                    result
                };

                Some(response_ok(
                    id,
                    command_type,
                    Some(json!({
                        "model": rpc_model_from_entry(&entry),
                        "thinkingLevel": thinking_level.to_string(),
                        "isScoped": is_scoped,
                    })),
                ))
            }
            "set_thinking_level" => {
                let Some(level) = parsed.get("level").and_then(Value::as_str) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing level".to_string(),
                    )));
                };
                let level: ThinkingLevel = match level.parse() {
                    Ok(level) => level,
                    Err(err) => {
                        return Ok(Some(response_error_with_hints(
                            id,
                            command_type,
                            &Error::validation(err),
                        )));
                    }
                };

                {
                    let mut guard = ctx
                        .session
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let level = {
                        let inner_session = guard.session.lock(ctx.cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        current_model_entry(&inner_session, ctx.options)
                            .map_or(level, |entry| entry.clamp_thinking_level(level))
                    };
                    if let Err(err) = apply_thinking_level(&mut guard, level).await {
                        return Ok(Some(response_error_with_hints(id, command_type, &err)));
                    }
                }
                Some(response_ok(id, command_type, None))
            }
            "cycle_thinking_level" => {
                let next = {
                    let mut guard = ctx
                        .session
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let entry = {
                        let inner_session = guard.session.lock(ctx.cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        current_model_entry(&inner_session, ctx.options).cloned()
                    };
                    let Some(entry) = entry else {
                        return Ok(Some(response_ok(id, command_type, Some(Value::Null))));
                    };
                    if !entry.model.reasoning {
                        return Ok(Some(response_ok(id, command_type, Some(Value::Null))));
                    }

                    let levels = available_thinking_levels(&entry);
                    let current = guard
                        .agent
                        .stream_options()
                        .thinking_level
                        .unwrap_or_default();
                    let current_index = levels
                        .iter()
                        .position(|level| *level == current)
                        .unwrap_or(0);
                    let next = levels[(current_index + 1) % levels.len()];
                    apply_thinking_level(&mut guard, next).await?;
                    next
                };
                Some(response_ok(
                    id,
                    command_type,
                    Some(json!({ "level": next.to_string() })),
                ))
            }
            "set_steering_mode" => {
                let Some(mode) = parsed.get("mode").and_then(Value::as_str) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing mode".to_string(),
                    )));
                };
                let Some(mode) = parse_queue_mode(Some(mode)) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Invalid steering mode".to_string(),
                    )));
                };
                let mut state = ctx
                    .shared_state
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.set_steering_mode(mode);
                drop(state);
                sync_agent_queue_modes(ctx.session, ctx.shared_state, ctx.cx).await?;
                Some(response_ok(id, command_type, None))
            }
            "set_follow_up_mode" => {
                let Some(mode) = parsed.get("mode").and_then(Value::as_str) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing mode".to_string(),
                    )));
                };
                let Some(mode) = parse_queue_mode(Some(mode)) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Invalid follow-up mode".to_string(),
                    )));
                };
                let mut state = ctx
                    .shared_state
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.set_follow_up_mode(mode);
                drop(state);
                sync_agent_queue_modes(ctx.session, ctx.shared_state, ctx.cx).await?;
                Some(response_ok(id, command_type, None))
            }
            "set_auto_compaction" => {
                let Some(enabled) = parsed.get("enabled").and_then(Value::as_bool) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing enabled".to_string(),
                    )));
                };
                let mut state = ctx
                    .shared_state
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.set_auto_compaction_enabled(enabled);
                Some(response_ok(id, command_type, None))
            }
            "set_auto_retry" => {
                let Some(enabled) = parsed.get("enabled").and_then(Value::as_bool) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing enabled".to_string(),
                    )));
                };
                let mut state = ctx
                    .shared_state
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.set_auto_retry_enabled(enabled);
                Some(response_ok(id, command_type, None))
            }
            "abort_retry" => {
                ctx.retry_abort.store(true, Ordering::SeqCst);
                Some(response_ok(id, command_type, None))
            }
            "set_session_name" => {
                let Some(name) = parsed.get("name").and_then(Value::as_str) else {
                    return Ok(Some(response_error(
                        id,
                        command_type,
                        "Missing name".to_string(),
                    )));
                };
                {
                    let mut guard = ctx
                        .session
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    {
                        let mut inner_session =
                            guard.session.lock(ctx.cx).await.map_err(|err| {
                                Error::session(format!("inner session lock failed: {err}"))
                            })?;
                        inner_session.append_session_info(Some(name.to_string()));
                    }
                    guard.persist_session().await?;
                }
                Some(response_ok(id, command_type, None))
            }
            "get_last_assistant_text" => {
                let text = {
                    let inner_session = ctx.session_handle.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    last_assistant_text(&inner_session)
                };
                Some(response_ok(id, command_type, Some(json!({ "text": text }))))
            }
            "export_html" => {
                let output_path = parsed
                    .get("outputPath")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let snapshot = {
                    let guard = ctx
                        .session
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner = guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner.export_snapshot()
                };
                let path = export_html_snapshot(&snapshot, output_path.as_deref()).await?;
                Some(response_ok(id, command_type, Some(json!({ "path": path }))))
            }
            "get_commands" => {
                let commands = ctx.options.resources.list_commands();
                Some(response_ok(
                    id,
                    command_type,
                    Some(json!({ "commands": commands })),
                ))
            }
            _ => None,
        };

    Ok(response)
}
