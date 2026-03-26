use crate::agent::{AgentSession, QueueMode};
use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extensions::ExtensionUiRequest;
use crate::model::{ContentBlock, Message};
use crate::models::ModelEntry;
use crate::provider_metadata::provider_metadata;
use crate::session::SessionMessage;
use crate::surface::rpc_types::{RpcOptions, provider_ids_match};
use asupersync::channel::oneshot;
use asupersync::sync::Mutex;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) const MAX_RPC_PENDING_MESSAGES: usize = 128;

#[derive(Debug, Clone)]
pub(crate) struct RpcStateSnapshot {
    pub(crate) steering_count: usize,
    pub(crate) follow_up_count: usize,
    pub(crate) steering_mode: QueueMode,
    pub(crate) follow_up_mode: QueueMode,
    pub(crate) auto_compaction_enabled: bool,
    pub(crate) auto_retry_enabled: bool,
}

#[derive(Debug)]
pub(crate) struct RpcSharedState {
    pub(crate) steering: VecDeque<Message>,
    pub(crate) follow_up: VecDeque<Message>,
    pub(crate) steering_mode: QueueMode,
    pub(crate) follow_up_mode: QueueMode,
    pub(crate) auto_compaction_enabled: bool,
    pub(crate) auto_retry_enabled: bool,
}

impl From<&RpcSharedState> for RpcStateSnapshot {
    fn from(state: &RpcSharedState) -> Self {
        Self {
            steering_count: state.steering.len(),
            follow_up_count: state.follow_up.len(),
            steering_mode: state.steering_mode,
            follow_up_mode: state.follow_up_mode,
            auto_compaction_enabled: state.auto_compaction_enabled,
            auto_retry_enabled: state.auto_retry_enabled,
        }
    }
}

impl RpcStateSnapshot {
    pub(crate) const fn pending_count(&self) -> usize {
        self.steering_count + self.follow_up_count
    }
}

impl RpcSharedState {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode: config.steering_queue_mode(),
            follow_up_mode: config.follow_up_queue_mode(),
            auto_compaction_enabled: config.compaction_enabled(),
            auto_retry_enabled: config.retry_enabled(),
        }
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.steering.len() + self.follow_up.len()
    }

    pub(crate) fn push_steering(&mut self, message: Message) -> Result<()> {
        if self.steering.len() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session(
                "Steering queue is full (Do you have too many pending commands?)",
            ));
        }
        self.steering.push_back(message);
        Ok(())
    }

    pub(crate) fn push_follow_up(&mut self, message: Message) -> Result<()> {
        if self.follow_up.len() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session("Follow-up queue is full"));
        }
        self.follow_up.push_back(message);
        Ok(())
    }

    pub(crate) fn pop_steering(&mut self) -> Vec<Message> {
        match self.steering_mode {
            QueueMode::All => self.steering.drain(..).collect(),
            QueueMode::OneAtATime => self.steering.pop_front().into_iter().collect(),
        }
    }

    pub(crate) fn pop_follow_up(&mut self) -> Vec<Message> {
        match self.follow_up_mode {
            QueueMode::All => self.follow_up.drain(..).collect(),
            QueueMode::OneAtATime => self.follow_up.pop_front().into_iter().collect(),
        }
    }

    pub(crate) const fn set_steering_mode(&mut self, mode: QueueMode) {
        self.steering_mode = mode;
    }

    pub(crate) const fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up_mode = mode;
    }

    pub(crate) const fn set_auto_compaction_enabled(&mut self, enabled: bool) {
        self.auto_compaction_enabled = enabled;
    }

    pub(crate) const fn set_auto_retry_enabled(&mut self, enabled: bool) {
        self.auto_retry_enabled = enabled;
    }

    pub(crate) const fn auto_compaction_enabled(&self) -> bool {
        self.auto_compaction_enabled
    }

    pub(crate) const fn auto_retry_enabled(&self) -> bool {
        self.auto_retry_enabled
    }

    pub(crate) fn clear_pending(&mut self) {
        self.steering.clear();
        self.follow_up.clear();
    }
}

pub(crate) async fn sync_agent_queue_modes(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    cx: &AgentCx,
) -> Result<()> {
    let (steering_mode, follow_up_mode) = {
        let state = shared_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
        (state.steering_mode, state.follow_up_mode)
    };

    let mut guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    guard.agent.set_queue_modes(steering_mode, follow_up_mode);
    Ok(())
}

/// Tracks a running bash command so it can be aborted.
pub(crate) struct RunningBash {
    pub(crate) id: String,
    pub(crate) abort_tx: oneshot::Sender<()>,
}

#[derive(Debug, Default)]
pub(crate) struct RpcUiBridgeState {
    pub(crate) active: Option<ExtensionUiRequest>,
    pub(crate) queue: VecDeque<ExtensionUiRequest>,
}

pub(crate) fn rpc_session_message_value(message: SessionMessage) -> Value {
    let mut value =
        serde_json::to_value(message).expect("SessionMessage should always serialize to JSON");
    rpc_flatten_content_blocks(&mut value);
    value
}

pub(crate) fn rpc_flatten_content_blocks(value: &mut Value) {
    let Value::Object(message_obj) = value else {
        return;
    };
    let Some(content) = message_obj.get_mut("content") else {
        return;
    };
    let Value::Array(blocks) = content else {
        return;
    };

    for block in blocks {
        let Value::Object(block_obj) = block else {
            continue;
        };
        let Some(inner) = block_obj.remove("0") else {
            continue;
        };
        let Value::Object(inner_obj) = inner else {
            block_obj.insert("0".to_string(), inner);
            continue;
        };
        for (key, value) in inner_obj {
            block_obj.entry(key).or_insert(value);
        }
    }
}

pub(crate) fn rpc_model_from_entry(entry: &ModelEntry) -> Value {
    let input = entry
        .model
        .input
        .iter()
        .map(|t| match t {
            crate::provider::InputType::Text => "text",
            crate::provider::InputType::Image => "image",
        })
        .collect::<Vec<_>>();

    json!({
        "id": entry.model.id,
        "name": entry.model.name,
        "api": entry.model.api,
        "provider": entry.model.provider,
        "baseUrl": entry.model.base_url,
        "reasoning": entry.model.reasoning,
        "input": input,
        "contextWindow": entry.model.context_window,
        "maxTokens": entry.model.max_tokens,
        "cost": entry.model.cost,
    })
}

pub(crate) fn session_state(
    session: &crate::session::Session,
    options: &RpcOptions,
    snapshot: &RpcStateSnapshot,
    is_streaming: bool,
    is_compacting: bool,
) -> Value {
    let model = session
        .header
        .provider
        .as_deref()
        .zip(session.header.model_id.as_deref())
        .and_then(|(provider, model_id)| {
            options.available_models.iter().find(|m| {
                provider_ids_match(&m.model.provider, provider)
                    && m.model.id.eq_ignore_ascii_case(model_id)
            })
        })
        .map(rpc_model_from_entry);

    let message_count = session
        .entries_for_current_path()
        .iter()
        .filter(|entry| matches!(entry, crate::session::SessionEntry::Message(_)))
        .count();

    let session_name = session
        .entries_for_current_path()
        .iter()
        .rev()
        .find_map(|entry| {
            let crate::session::SessionEntry::SessionInfo(info) = entry else {
                return None;
            };
            info.name.clone()
        });

    let mut state = serde_json::Map::new();
    state.insert("model".to_string(), model.unwrap_or(Value::Null));
    state.insert(
        "thinkingLevel".to_string(),
        Value::String(
            session
                .header
                .thinking_level
                .clone()
                .unwrap_or_else(|| "off".to_string()),
        ),
    );
    state.insert("isStreaming".to_string(), Value::Bool(is_streaming));
    state.insert("isCompacting".to_string(), Value::Bool(is_compacting));
    state.insert(
        "steeringMode".to_string(),
        Value::String(snapshot.steering_mode.as_str().to_string()),
    );
    state.insert(
        "followUpMode".to_string(),
        Value::String(snapshot.follow_up_mode.as_str().to_string()),
    );
    state.insert(
        "sessionFile".to_string(),
        session
            .path
            .as_ref()
            .map_or(Value::Null, |p| Value::String(p.display().to_string())),
    );
    state.insert(
        "sessionId".to_string(),
        Value::String(session.header.id.clone()),
    );
    state.insert(
        "sessionName".to_string(),
        session_name.map_or(Value::Null, Value::String),
    );
    state.insert(
        "autoCompactionEnabled".to_string(),
        Value::Bool(snapshot.auto_compaction_enabled),
    );
    state.insert(
        "messageCount".to_string(),
        Value::Number(message_count.into()),
    );
    state.insert(
        "pendingMessageCount".to_string(),
        Value::Number(snapshot.pending_count().into()),
    );
    state.insert(
        "durabilityMode".to_string(),
        Value::String(session.autosave_durability_mode().as_str().to_string()),
    );
    Value::Object(state)
}

pub(crate) fn session_stats(session: &crate::session::Session) -> Value {
    let mut user_messages: u64 = 0;
    let mut assistant_messages: u64 = 0;
    let mut tool_results: u64 = 0;
    let mut tool_calls: u64 = 0;

    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_cache_read: u64 = 0;
    let mut total_cache_write: u64 = 0;
    let mut total_cost: f64 = 0.0;

    let messages = session.to_messages_for_current_path();

    for message in &messages {
        match message {
            Message::User(_) | Message::Custom(_) => user_messages += 1,
            Message::Assistant(message) => {
                assistant_messages += 1;
                tool_calls += message
                    .content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
                    .count() as u64;
                total_input += message.usage.input;
                total_output += message.usage.output;
                total_cache_read += message.usage.cache_read;
                total_cache_write += message.usage.cache_write;
                total_cost += message.usage.cost.total;
            }
            Message::ToolResult(_) => tool_results += 1,
        }
    }

    let total_messages = messages.len() as u64;

    let total_tokens = total_input + total_output + total_cache_read + total_cache_write;
    let autosave = session.autosave_metrics();
    let pending_message_count = autosave.pending_mutations as u64;
    let durability_mode = session.autosave_durability_mode();
    let durability_mode_label = match durability_mode {
        crate::session::AutosaveDurabilityMode::Strict => "strict",
        crate::session::AutosaveDurabilityMode::Balanced => "balanced",
        crate::session::AutosaveDurabilityMode::Throughput => "throughput",
    };
    let (status_event, status_severity, status_summary, status_action, status_sli_ids) =
        if pending_message_count == 0 {
            (
                "session.persistence.healthy",
                "ok",
                "Persistence queue is clear.",
                "No action required.",
                vec!["sli_resume_ready_p95_ms"],
            )
        } else {
            let summary = match durability_mode {
                crate::session::AutosaveDurabilityMode::Strict => {
                    "Pending persistence backlog under strict durability mode."
                }
                crate::session::AutosaveDurabilityMode::Balanced => {
                    "Pending persistence backlog under balanced durability mode."
                }
                crate::session::AutosaveDurabilityMode::Throughput => {
                    "Pending persistence backlog under throughput durability mode."
                }
            };
            let action = match durability_mode {
                crate::session::AutosaveDurabilityMode::Throughput => {
                    "Expect deferred writes; trigger manual save before critical transitions."
                }
                _ => "Allow autosave flush to complete or trigger manual save before exit.",
            };
            (
                "session.persistence.backlog",
                "warning",
                summary,
                action,
                vec![
                    "sli_resume_ready_p95_ms",
                    "sli_failure_recovery_success_rate",
                ],
            )
        };

    let mut data = serde_json::Map::new();
    data.insert(
        "sessionFile".to_string(),
        session
            .path
            .as_ref()
            .map_or(Value::Null, |p| Value::String(p.display().to_string())),
    );
    data.insert(
        "sessionId".to_string(),
        Value::String(session.header.id.clone()),
    );
    data.insert(
        "userMessages".to_string(),
        Value::Number(user_messages.into()),
    );
    data.insert(
        "assistantMessages".to_string(),
        Value::Number(assistant_messages.into()),
    );
    data.insert("toolCalls".to_string(), Value::Number(tool_calls.into()));
    data.insert(
        "toolResults".to_string(),
        Value::Number(tool_results.into()),
    );
    data.insert(
        "totalMessages".to_string(),
        Value::Number(total_messages.into()),
    );
    data.insert(
        "durabilityMode".to_string(),
        Value::String(durability_mode_label.to_string()),
    );
    data.insert(
        "pendingMessageCount".to_string(),
        Value::Number(pending_message_count.into()),
    );
    data.insert(
        "tokens".to_string(),
        json!({
            "input": total_input,
            "output": total_output,
            "cacheRead": total_cache_read,
            "cacheWrite": total_cache_write,
            "total": total_tokens,
        }),
    );
    data.insert(
        "persistenceStatus".to_string(),
        json!({
            "event": status_event,
            "severity": status_severity,
            "summary": status_summary,
            "action": status_action,
            "sliIds": status_sli_ids,
            "pendingMessageCount": pending_message_count,
            "flushCounters": {
                "started": autosave.flush_started,
                "succeeded": autosave.flush_succeeded,
                "failed": autosave.flush_failed,
            },
        }),
    );
    data.insert(
        "uxEventMarkers".to_string(),
        json!([
            {
                "event": status_event,
                "severity": status_severity,
                "durabilityMode": durability_mode_label,
                "pendingMessageCount": pending_message_count,
                "sliIds": status_sli_ids,
            }
        ]),
    );
    data.insert("cost".to_string(), Value::from(total_cost));
    Value::Object(data)
}

pub(crate) fn last_assistant_text(session: &crate::session::Session) -> Option<String> {
    let entries = session.entries_for_current_path();
    for entry in entries.into_iter().rev() {
        let crate::session::SessionEntry::Message(msg_entry) = entry else {
            continue;
        };
        let SessionMessage::Assistant { message } = &msg_entry.message else {
            continue;
        };
        let mut text = String::new();
        for block in &message.content {
            if let ContentBlock::Text(t) = block {
                text.push_str(&t.text);
            }
        }
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

pub(crate) async fn export_html_snapshot(
    snapshot: &crate::session::ExportSnapshot,
    output_path: Option<&str>,
) -> Result<String> {
    let html = snapshot.to_html();

    let path = output_path.map_or_else(
        || {
            snapshot.path.as_ref().map_or_else(
                || {
                    let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S%.3fZ");
                    PathBuf::from(format!("pi-session-{ts}.html"))
                },
                |session_path| {
                    let basename = session_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("session");
                    PathBuf::from(format!("pi-session-{basename}.html"))
                },
            )
        },
        PathBuf::from,
    );

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        asupersync::fs::create_dir_all(parent).await?;
    }
    asupersync::fs::write(&path, html).await?;
    Ok(path.display().to_string())
}

pub(crate) fn resolve_model_key(auth: &AuthStorage, entry: &ModelEntry) -> Option<String> {
    normalize_api_key_opt(auth.resolve_api_key_for_model(
        &entry.model.provider,
        Some(&entry.model.id),
        None,
    ))
    .or_else(|| normalize_api_key_opt(entry.api_key.clone()))
}

fn normalize_api_key_opt(api_key: Option<String>) -> Option<String> {
    api_key.and_then(|key| {
        let trimmed = key.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

pub(crate) fn model_requires_configured_credential(entry: &ModelEntry) -> bool {
    let provider = entry.model.provider.as_str();
    entry.auth_header
        || provider_metadata(provider).is_some_and(|meta| !meta.auth_env_keys.is_empty())
        || entry.oauth_config.is_some()
}

pub(crate) fn current_model_entry<'a>(
    session: &crate::session::Session,
    options: &'a RpcOptions,
) -> Option<&'a ModelEntry> {
    let provider = session.header.provider.as_deref()?;
    let model_id = session.header.model_id.as_deref()?;
    options.available_models.iter().find(|m| {
        provider_ids_match(&m.model.provider, provider) && m.model.id.eq_ignore_ascii_case(model_id)
    })
}

pub(crate) async fn apply_thinking_level(
    guard: &mut AgentSession,
    level: crate::model::ThinkingLevel,
) -> Result<()> {
    let cx = AgentCx::for_request();
    {
        let mut inner_session = guard
            .session
            .lock(cx.cx())
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        inner_session.header.thinking_level = Some(level.to_string());
        inner_session.append_thinking_level_change(level.to_string());
    }
    guard.agent.stream_options_mut().thinking_level = Some(level);
    guard.persist_session().await
}

pub(crate) async fn apply_model_change(guard: &mut AgentSession, entry: &ModelEntry) -> Result<()> {
    let cx = AgentCx::for_request();
    {
        let mut inner_session = guard
            .session
            .lock(cx.cx())
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        inner_session.header.provider = Some(entry.model.provider.clone());
        inner_session.header.model_id = Some(entry.model.id.clone());
        inner_session.append_model_change(entry.model.provider.clone(), entry.model.id.clone());
    }
    guard.persist_session().await
}

pub(crate) fn fork_messages_from_entries(entries: &[crate::session::SessionEntry]) -> Vec<Value> {
    let mut result = Vec::new();

    for entry in entries {
        let crate::session::SessionEntry::Message(m) = entry else {
            continue;
        };
        let SessionMessage::User { content, .. } = &m.message else {
            continue;
        };
        let entry_id = m.base.id.clone().unwrap_or_default();
        let text = extract_user_text(content);
        result.push(json!({
            "entryId": entry_id,
            "text": text,
        }));
    }

    result
}

pub(crate) fn extract_user_text(content: &crate::model::UserContent) -> Option<String> {
    match content {
        crate::model::UserContent::Text(text) => Some(text.clone()),
        crate::model::UserContent::Blocks(blocks) => blocks.iter().find_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.clone())
            } else {
                None
            }
        }),
    }
}

pub(crate) fn available_thinking_levels(entry: &ModelEntry) -> Vec<crate::model::ThinkingLevel> {
    use crate::model::ThinkingLevel;
    if entry.model.reasoning {
        let mut levels = vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ];
        if entry.supports_xhigh() {
            levels.push(ThinkingLevel::XHigh);
        }
        levels
    } else {
        vec![ThinkingLevel::Off]
    }
}

pub(crate) async fn cycle_model_for_rpc(
    guard: &mut AgentSession,
    options: &RpcOptions,
) -> Result<Option<(ModelEntry, crate::model::ThinkingLevel, bool)>> {
    let (candidates, is_scoped) = if options.scoped_models.is_empty() {
        (options.available_models.clone(), false)
    } else {
        (
            options
                .scoped_models
                .iter()
                .map(|sm| sm.model.clone())
                .collect::<Vec<_>>(),
            true,
        )
    };

    if candidates.len() <= 1 {
        return Ok(None);
    }

    let cx = AgentCx::for_request();
    let (current_provider, current_model_id) = {
        let inner_session = guard
            .session
            .lock(cx.cx())
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        (
            inner_session.header.provider.clone(),
            inner_session.header.model_id.clone(),
        )
    };

    let current_index = candidates.iter().position(|entry| {
        current_provider
            .as_deref()
            .is_some_and(|provider| provider_ids_match(provider, &entry.model.provider))
            && current_model_id
                .as_deref()
                .is_some_and(|model_id| model_id.eq_ignore_ascii_case(&entry.model.id))
    });

    let next_index = current_index.map_or(0, |idx| (idx + 1) % candidates.len());

    let next_entry = candidates[next_index].clone();
    let provider_impl = crate::providers::create_provider(
        &next_entry,
        guard
            .extensions
            .as_ref()
            .map(crate::extensions::ExtensionRegion::manager),
    )?;
    guard.agent.set_provider(provider_impl);

    let key = resolve_model_key(&options.auth, &next_entry);
    if model_requires_configured_credential(&next_entry) && key.is_none() {
        return Err(Error::auth(format!(
            "Missing credentials for {}/{}",
            next_entry.model.provider, next_entry.model.id
        )));
    }
    guard.agent.stream_options_mut().api_key.clone_from(&key);
    guard
        .agent
        .stream_options_mut()
        .headers
        .clone_from(&next_entry.headers);

    apply_model_change(guard, &next_entry).await?;

    let desired_thinking = if is_scoped {
        options.scoped_models[next_index]
            .thinking_level
            .unwrap_or(crate::model::ThinkingLevel::Off)
    } else {
        guard
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default()
    };

    let next_thinking = next_entry.clamp_thinking_level(desired_thinking);
    apply_thinking_level(guard, next_thinking).await?;

    Ok(Some((next_entry, next_thinking, is_scoped)))
}
