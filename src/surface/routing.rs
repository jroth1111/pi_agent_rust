use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::agent::{AbortHandle, AgentEvent, AgentSession};
use crate::auth::AuthStorage;
use crate::config::Config;
use crate::model::{AssistantMessage, ContentBlock, StopReason};
use crate::models::ModelEntry;
use crate::resources::{ResourceCliOptions, ResourceLoader};
use anyhow::{Result, bail};
use asupersync::runtime::RuntimeHandle;

type InitialMessage = crate::app::InitialMessage;

async fn run_rpc_mode(
    session: AgentSession,
    resources: ResourceLoader,
    config: Config,
    available_models: Vec<ModelEntry>,
    scoped_models: Vec<crate::rpc::RpcScopedModel>,
    auth: AuthStorage,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    use futures::FutureExt;

    let (abort_handle, abort_signal) = AbortHandle::new();
    let abort_listener = abort_handle.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        abort_listener.abort();
    }) {
        eprintln!("Warning: Failed to install Ctrl+C handler for RPC mode: {err}");
    }
    let rpc_task = crate::rpc::run_stdio(
        session,
        crate::rpc::RpcOptions {
            config,
            resources,
            available_models,
            scoped_models,
            auth,
            runtime_handle,
        },
    )
    .fuse();

    let signal_task = abort_signal.wait().fuse();

    futures::pin_mut!(rpc_task, signal_task);

    match futures::future::select(rpc_task, signal_task).await {
        futures::future::Either::Left((result, _)) => match result {
            Ok(()) => Ok(()),
            Err(err) => Err(anyhow::Error::new(err)),
        },
        futures::future::Either::Right(((), _)) => Ok(()),
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_print_mode(
    session: &mut AgentSession,
    mode: &str,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    resources: &ResourceLoader,
    runtime_handle: RuntimeHandle,
    config: &Config,
) -> Result<()> {
    if mode != "text" && mode != "json" {
        bail!("Unknown mode: {mode}");
    }

    let extension_ui_blocked =
        crate::surface::install_non_interactive_extension_ui_bridge(session, &runtime_handle);

    if mode == "json" {
        let cx = crate::agent_cx::AgentCx::for_request();
        let session = session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("{}", serde_json::to_string(&session.header)?);
    }
    if initial.is_none() && messages.is_empty() {
        if mode == "json" {
            io::stdout().flush()?;
            return Ok(());
        }
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    let mut last_message: Option<AssistantMessage> = None;
    let extensions = session.extensions.as_ref().map(|r| r.manager().clone());
    let emit_json_events = mode == "json";
    let runtime_for_events = runtime_handle.clone();
    let make_event_handler = move || {
        let extensions = extensions.clone();
        let runtime_for_events = runtime_for_events.clone();
        let coalescer = extensions
            .as_ref()
            .map(|m| crate::extensions::EventCoalescer::new(m.clone()));
        move |event: AgentEvent| {
            if emit_json_events && let Ok(serialized) = serde_json::to_string(&event) {
                println!("{serialized}");
            }
            if let Some(coal) = &coalescer {
                coal.dispatch_agent_event_lazy(&event, &runtime_for_events);
            }
        }
    };
    let (abort_handle, abort_signal) = AbortHandle::new();
    let abort_listener = abort_handle.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        abort_listener.abort();
    }) {
        eprintln!("Warning: Failed to install Ctrl+C handler: {err}");
    }

    let mut initial = initial;
    if let Some(ref mut initial) = initial {
        initial.text = resources.expand_input(&initial.text);
    }

    let messages = messages
        .into_iter()
        .map(|message| resources.expand_input(&message))
        .collect::<Vec<_>>();

    let retry_enabled = config.retry_enabled();
    let max_retries = config.retry_max_retries();
    let is_json = mode == "json";

    if let Some(initial) = initial {
        let content = crate::app::build_initial_content(&initial);
        last_message = Some(
            run_print_prompt_with_retry(
                session,
                config,
                &abort_signal,
                &make_event_handler,
                retry_enabled,
                max_retries,
                is_json,
                PromptInput::Content(content),
            )
            .await?,
        );
        crate::surface::check_non_interactive_extension_ui_blocked(&extension_ui_blocked)?;
    }

    for message in messages {
        last_message = Some(
            run_print_prompt_with_retry(
                session,
                config,
                &abort_signal,
                &make_event_handler,
                retry_enabled,
                max_retries,
                is_json,
                PromptInput::Text(message),
            )
            .await?,
        );
        crate::surface::check_non_interactive_extension_ui_blocked(&extension_ui_blocked)?;
    }

    let Some(last_message) = last_message else {
        if mode == "json" {
            io::stdout().flush()?;
            return Ok(());
        }
        bail!("No messages were sent");
    };

    if mode == "text" {
        if matches!(
            last_message.stop_reason,
            StopReason::Error | StopReason::Aborted
        ) {
            let message = last_message
                .error_message
                .unwrap_or_else(|| "Request error".to_string());
            bail!(message);
        }
        if std::io::IsTerminal::is_terminal(&io::stdout()) {
            let mut markdown = String::new();
            for block in &last_message.content {
                if let ContentBlock::Text(text) = block {
                    markdown.push_str(&text.text);
                    if !markdown.ends_with('\n') {
                        markdown.push('\n');
                    }
                }
            }

            if !markdown.is_empty() {
                let console = crate::tui::PiConsole::new();
                console.render_markdown(&markdown);
            }
        } else {
            crate::app::output_final_text(&last_message);
        }
    }

    io::stdout().flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_selected_surface_route(
    route_kind: crate::surface::CliRouteKind,
    mut agent_session: AgentSession,
    selection: &crate::app::ModelSelection,
    available_models: Vec<ModelEntry>,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    resources: ResourceLoader,
    config: &Config,
    auth: AuthStorage,
    runtime_handle: RuntimeHandle,
    resource_cli: ResourceCliOptions,
    cwd: PathBuf,
    save_enabled: bool,
    mode: &str,
) -> Result<()> {
    match route_kind {
        crate::surface::CliRouteKind::Rpc => {
            let rpc_scoped_models = selection
                .scoped_models
                .iter()
                .map(|sm| crate::rpc::RpcScopedModel {
                    model: sm.model.clone(),
                    thinking_level: sm.thinking_level,
                })
                .collect::<Vec<_>>();
            Box::pin(run_rpc_mode(
                agent_session,
                resources,
                config.clone(),
                available_models,
                rpc_scoped_models,
                auth,
                runtime_handle,
            ))
            .await
        }
        crate::surface::CliRouteKind::Interactive => {
            let model_scope = selection
                .scoped_models
                .iter()
                .map(|sm| sm.model.clone())
                .collect::<Vec<_>>();

            run_interactive_mode(
                agent_session,
                initial,
                messages,
                config.clone(),
                selection.model_entry.clone(),
                model_scope,
                available_models,
                save_enabled,
                resources,
                resource_cli,
                cwd,
                runtime_handle,
            )
            .await
        }
        crate::surface::CliRouteKind::Print
        | crate::surface::CliRouteKind::Json
        | crate::surface::CliRouteKind::Text => {
            let result = run_print_mode(
                &mut agent_session,
                mode,
                initial,
                messages,
                &resources,
                runtime_handle,
                config,
            )
            .await;
            if let Some(ref ext) = agent_session.extensions {
                ext.shutdown().await;
            }
            result
        }
        crate::surface::CliRouteKind::Subcommand | crate::surface::CliRouteKind::FastOffline => {
            unreachable!("subcommands and fast-offline routes do not enter run()")
        }
    }
}

enum PromptInput {
    Text(String),
    Content(Vec<ContentBlock>),
}

fn print_mode_retry_delay_ms(config: &Config, attempt: u32) -> u32 {
    let base = u64::from(config.retry_base_delay_ms());
    let max = u64::from(config.retry_max_delay_ms());
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
}

fn emit_json_event(event: &AgentEvent) {
    if let Ok(serialized) = serde_json::to_string(event) {
        println!("{serialized}");
    }
}

fn is_retryable_prompt_result(msg: &AssistantMessage) -> bool {
    if !matches!(msg.stop_reason, StopReason::Error) {
        return false;
    }
    let err_msg = msg.error_message.as_deref().unwrap_or("Request error");
    crate::error::is_retryable_error(err_msg, Some(msg.usage.input), None)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_print_prompt_with_retry<H, EH>(
    session: &mut AgentSession,
    config: &Config,
    abort_signal: &crate::agent::AbortSignal,
    make_event_handler: &H,
    retry_enabled: bool,
    max_retries: u32,
    is_json: bool,
    input: PromptInput,
) -> Result<AssistantMessage>
where
    H: Fn() -> EH + Sync,
    EH: Fn(AgentEvent) + Send + Sync + 'static,
{
    let first_result = match &input {
        PromptInput::Text(text) => {
            session
                .run_text_with_abort(
                    text.clone(),
                    Some(abort_signal.clone()),
                    make_event_handler(),
                )
                .await
        }
        PromptInput::Content(content) => {
            session
                .run_with_content_with_abort(
                    content.clone(),
                    Some(abort_signal.clone()),
                    make_event_handler(),
                )
                .await
        }
    };

    if !retry_enabled {
        return first_result.map_err(anyhow::Error::new);
    }

    let mut retry_count: u32 = 0;
    let mut current_result = first_result;

    loop {
        match current_result {
            Ok(msg) if msg.stop_reason == StopReason::Aborted => {
                if retry_count > 0 && is_json {
                    emit_json_event(&AgentEvent::AutoRetryEnd {
                        success: false,
                        attempt: retry_count,
                        final_error: Some("Aborted".to_string()),
                    });
                }
                return Ok(msg);
            }
            Ok(msg) if is_retryable_prompt_result(&msg) && retry_count < max_retries => {
                let err_msg = msg
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Request error".to_string());

                retry_count += 1;
                let delay_ms = print_mode_retry_delay_ms(config, retry_count);
                if is_json {
                    emit_json_event(&AgentEvent::AutoRetryStart {
                        attempt: retry_count,
                        max_attempts: max_retries,
                        delay_ms: u64::from(delay_ms),
                        error_message: err_msg,
                    });
                }

                asupersync::time::sleep(
                    asupersync::time::wall_now(),
                    Duration::from_millis(u64::from(delay_ms)),
                )
                .await;

                current_result = match &input {
                    PromptInput::Text(text) => {
                        session
                            .run_text_with_abort(
                                text.clone(),
                                Some(abort_signal.clone()),
                                make_event_handler(),
                            )
                            .await
                    }
                    PromptInput::Content(content) => {
                        session
                            .run_with_content_with_abort(
                                content.clone(),
                                Some(abort_signal.clone()),
                                make_event_handler(),
                            )
                            .await
                    }
                };
            }
            Ok(msg) => {
                let success = !matches!(msg.stop_reason, StopReason::Error);
                if retry_count > 0 && is_json {
                    emit_json_event(&AgentEvent::AutoRetryEnd {
                        success,
                        attempt: retry_count,
                        final_error: if success {
                            None
                        } else {
                            msg.error_message.clone()
                        },
                    });
                }
                return Ok(msg);
            }
            Err(err) => {
                let err_str = err.to_string();
                if retry_count < max_retries
                    && crate::error::is_retryable_error(&err_str, None, None)
                {
                    retry_count += 1;
                    let delay_ms = print_mode_retry_delay_ms(config, retry_count);
                    if is_json {
                        emit_json_event(&AgentEvent::AutoRetryStart {
                            attempt: retry_count,
                            max_attempts: max_retries,
                            delay_ms: u64::from(delay_ms),
                            error_message: err_str,
                        });
                    }

                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(u64::from(delay_ms)),
                    )
                    .await;

                    current_result = match &input {
                        PromptInput::Text(text) => {
                            session
                                .run_text_with_abort(
                                    text.clone(),
                                    Some(abort_signal.clone()),
                                    make_event_handler(),
                                )
                                .await
                        }
                        PromptInput::Content(content) => {
                            session
                                .run_with_content_with_abort(
                                    content.clone(),
                                    Some(abort_signal.clone()),
                                    make_event_handler(),
                                )
                                .await
                        }
                    };
                } else {
                    if retry_count > 0 && is_json {
                        emit_json_event(&AgentEvent::AutoRetryEnd {
                            success: false,
                            attempt: retry_count,
                            final_error: Some(err_str),
                        });
                    }
                    return Err(anyhow::Error::new(err));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive_mode(
    session: AgentSession,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    config: Config,
    model_entry: ModelEntry,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    save_enabled: bool,
    resources: ResourceLoader,
    resource_cli: ResourceCliOptions,
    cwd: PathBuf,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    let mut pending = Vec::new();
    if let Some(initial) = initial {
        pending.push(crate::interactive::PendingInput::Content(
            crate::app::build_initial_content(&initial),
        ));
    }
    for message in messages {
        pending.push(crate::interactive::PendingInput::Text(message));
    }

    let AgentSession {
        agent,
        session,
        extensions: region,
        ..
    } = session;
    let extensions = region.as_ref().map(|r| r.manager().clone());
    let interactive_result = crate::interactive::run_interactive(
        agent,
        session,
        config,
        model_entry,
        model_scope,
        available_models,
        pending,
        save_enabled,
        resources,
        resource_cli,
        extensions,
        cwd,
        runtime_handle,
    )
    .await;
    if let Some(ref region) = region {
        region.shutdown().await;
    }
    interactive_result?;
    Ok(())
}
