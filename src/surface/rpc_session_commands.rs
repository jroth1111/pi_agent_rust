use crate::agent::AgentSession;
use crate::agent_cx::AgentCx;
use crate::compaction::{
    ResolvedCompactionSettings, compact, compaction_details_to_value, prepare_compaction,
};
use crate::error::{Error, Result};
use crate::rpc::RpcOptions;
use crate::surface::rpc_protocol::{response_error, response_error_with_hints, response_ok};
use crate::surface::rpc_support::{RpcSharedState, fork_messages_from_entries};
use asupersync::sync::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct RpcSessionCommandContext<'a> {
    pub(crate) cx: &'a AgentCx,
    pub(crate) session: &'a Arc<Mutex<AgentSession>>,
    pub(crate) shared_state: &'a Arc<Mutex<RpcSharedState>>,
    pub(crate) options: &'a RpcOptions,
    pub(crate) is_compacting: &'a Arc<AtomicBool>,
}

pub(crate) async fn handle_session_command(
    parsed: &Value,
    command_type: &str,
    id: Option<String>,
    ctx: RpcSessionCommandContext<'_>,
) -> Result<Option<String>> {
    let response = match command_type {
        "compact" => {
            let custom_instructions = parsed
                .get("customInstructions")
                .and_then(Value::as_str)
                .map(str::to_string);

            let data = {
                let mut guard = ctx
                    .session
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                let path_entries = {
                    let mut inner_session = guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner_session.ensure_entry_ids();
                    inner_session
                        .entries_for_current_path()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>()
                };

                let key = guard
                    .agent
                    .stream_options()
                    .api_key
                    .as_deref()
                    .ok_or_else(|| Error::auth("Missing API key for compaction"))?;

                let provider = guard.agent.provider();
                let settings = ResolvedCompactionSettings {
                    enabled: ctx.options.config.compaction_enabled(),
                    reserve_tokens: ctx.options.config.compaction_reserve_tokens(),
                    keep_recent_tokens: ctx.options.config.compaction_keep_recent_tokens(),
                    ..Default::default()
                };

                let prep = prepare_compaction(&path_entries, settings).ok_or_else(|| {
                    Error::session("Compaction not available (already compacted or missing IDs)")
                })?;

                ctx.is_compacting.store(true, Ordering::SeqCst);
                let result = compact(prep, provider, key, custom_instructions.as_deref()).await?;
                ctx.is_compacting.store(false, Ordering::SeqCst);
                let details_value = compaction_details_to_value(&result.details)?;

                let messages = {
                    let mut inner_session = guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner_session.append_compaction(
                        result.summary.clone(),
                        result.first_kept_entry_id.clone(),
                        result.tokens_before,
                        Some(details_value.clone()),
                        None,
                    );
                    inner_session.to_messages_for_current_path()
                };
                guard.persist_session().await?;
                guard.agent.replace_messages(messages);

                json!({
                    "summary": result.summary,
                    "firstKeptEntryId": result.first_kept_entry_id,
                    "tokensBefore": result.tokens_before,
                    "details": details_value,
                })
            };

            Some(response_ok(id, command_type, Some(data)))
        }
        "new_session" => {
            let parent = parsed
                .get("parentSession")
                .and_then(Value::as_str)
                .map(str::to_string);
            {
                let mut guard = ctx
                    .session
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                let (session_dir, provider, model_id, thinking_level) = {
                    let inner_session = guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    (
                        inner_session.session_dir.clone(),
                        inner_session.header.provider.clone(),
                        inner_session.header.model_id.clone(),
                        inner_session.header.thinking_level.clone(),
                    )
                };
                let mut new_session = if guard.save_enabled() {
                    crate::session::Session::create_with_dir(session_dir)
                } else {
                    crate::session::Session::in_memory()
                };
                new_session.header.parent_session = parent;
                new_session.header.provider.clone_from(&provider);
                new_session.header.model_id.clone_from(&model_id);
                new_session
                    .header
                    .thinking_level
                    .clone_from(&thinking_level);

                let session_id = new_session.header.id.clone();
                {
                    let mut inner_session = guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    *inner_session = new_session;
                }
                guard.agent.clear_messages();
                guard.agent.stream_options_mut().session_id = Some(session_id);
            }
            {
                let mut state = ctx
                    .shared_state
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.clear_pending();
            }
            Some(response_ok(
                id,
                command_type,
                Some(json!({ "cancelled": false })),
            ))
        }
        "switch_session" => {
            let Some(session_path) = parsed.get("sessionPath").and_then(Value::as_str) else {
                return Ok(Some(response_error(
                    id,
                    command_type,
                    "Missing sessionPath".to_string(),
                )));
            };

            Some(match crate::session::Session::open(session_path).await {
                Ok(new_session) => {
                    let messages = new_session.to_messages_for_current_path();
                    let session_id = new_session.header.id.clone();
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
                        *inner_session = new_session;
                    }
                    guard.agent.replace_messages(messages);
                    guard.agent.stream_options_mut().session_id = Some(session_id);
                    let mut state = ctx
                        .shared_state
                        .lock(ctx.cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.clear_pending();
                    response_ok(id, command_type, Some(json!({ "cancelled": false })))
                }
                Err(err) => response_error_with_hints(id, command_type, &err),
            })
        }
        "fork" => {
            let Some(entry_id) = parsed.get("entryId").and_then(Value::as_str) else {
                return Ok(Some(response_error(
                    id,
                    command_type,
                    "Missing entryId".to_string(),
                )));
            };

            let (fork_plan, parent_path, session_dir, save_enabled, header_snapshot) = {
                let guard = ctx
                    .session
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                let inner =
                    guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                let plan = inner.plan_fork_from_user_message(entry_id)?;
                let parent_path = inner.path.as_ref().map(|p| p.display().to_string());
                let session_dir = inner.session_dir.clone();
                let header = inner.header.clone();
                (plan, parent_path, session_dir, guard.save_enabled(), header)
            };

            let crate::session::ForkPlan {
                entries,
                leaf_id,
                selected_text,
            } = fork_plan;

            let mut new_session = if save_enabled {
                crate::session::Session::create_with_dir(session_dir)
            } else {
                crate::session::Session::in_memory()
            };
            new_session.header.parent_session = parent_path;
            new_session
                .header
                .provider
                .clone_from(&header_snapshot.provider);
            new_session
                .header
                .model_id
                .clone_from(&header_snapshot.model_id);
            new_session
                .header
                .thinking_level
                .clone_from(&header_snapshot.thinking_level);
            new_session.entries = entries;
            new_session.leaf_id = leaf_id;
            new_session.ensure_entry_ids();

            let messages = new_session.to_messages_for_current_path();
            let session_id = new_session.header.id.clone();

            {
                let mut guard = ctx
                    .session
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                let mut inner =
                    guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                *inner = new_session;
                drop(inner);
                guard.agent.replace_messages(messages);
                guard.agent.stream_options_mut().session_id = Some(session_id);
            }

            {
                let mut state = ctx
                    .shared_state
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.clear_pending();
            }

            Some(response_ok(
                id,
                command_type,
                Some(json!({ "text": selected_text, "cancelled": false })),
            ))
        }
        "get_fork_messages" => {
            let path_entries = {
                let guard = ctx
                    .session
                    .lock(ctx.cx)
                    .await
                    .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                let inner_session =
                    guard.session.lock(ctx.cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                inner_session
                    .entries_for_current_path()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            };
            let messages = fork_messages_from_entries(&path_entries);
            Some(response_ok(
                id,
                command_type,
                Some(json!({ "messages": messages })),
            ))
        }
        _ => None,
    };

    Ok(response)
}
