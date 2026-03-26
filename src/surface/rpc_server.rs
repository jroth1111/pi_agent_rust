use crate::agent::{AbortHandle, AgentSession};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::extensions::{ExtensionUiRequest, ExtensionUiResponse};
use crate::model::Message;
use crate::orchestration::RunStore;
use crate::surface::rpc_protocol::{normalize_command_type, response_error};
use crate::surface::rpc_runtime_commands::{self, RpcRuntimeCommandContext};
use crate::surface::rpc_service_commands::{self, RpcServiceCommandContext};
use crate::surface::rpc_session_commands::{self, RpcSessionCommandContext};
use crate::surface::rpc_support::{RpcSharedState, RpcUiBridgeState, RunningBash};
use crate::surface::rpc_transport_commands::{
    self, RpcTransportCommandContext, event, rpc_emit_extension_ui_request,
};
use crate::surface::rpc_types::{RpcOptions, RpcOrchestrationState, RpcReliabilityState};
use asupersync::channel::mpsc;
use asupersync::sync::Mutex;
use serde_json::Value;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub(crate) fn try_send_line_with_backpressure(tx: &mpsc::Sender<String>, mut line: String) -> bool {
    loop {
        match tx.try_send(line) {
            Ok(()) => return true,
            Err(mpsc::SendError::Full(unsent)) => {
                line = unsent;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(mpsc::SendError::Disconnected(_) | mpsc::SendError::Cancelled(_)) => {
                return false;
            }
        }
    }
}

pub async fn run_stdio(mut session: AgentSession, options: RpcOptions) -> Result<()> {
    session.agent.set_queue_modes(
        options.config.steering_queue_mode(),
        options.config.follow_up_queue_mode(),
    );

    let (in_tx, in_rx) = mpsc::channel::<String>(1024);
    let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();

    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = io::BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line_to_send = std::mem::take(&mut line);
                    if !try_send_line_with_backpressure(&in_tx, line_to_send) {
                        break;
                    }
                }
            }
        }
    });

    std::thread::spawn(move || {
        let stdout = io::stdout();
        let mut writer = io::BufWriter::new(stdout.lock());
        for line in out_rx {
            if writer.write_all(line.as_bytes()).is_err() {
                break;
            }
            if writer.write_all(b"\n").is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    Box::pin(run(session, options, in_rx, out_tx)).await
}

#[allow(clippy::too_many_lines)]
#[allow(
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]
pub async fn run(
    session: AgentSession,
    options: RpcOptions,
    in_rx: mpsc::Receiver<String>,
    out_tx: std::sync::mpsc::Sender<String>,
) -> Result<()> {
    let cx = AgentCx::for_request();
    let session_handle = Arc::clone(&session.session);
    let session = Arc::new(Mutex::new(session));
    let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&options.config)));
    let is_streaming = Arc::new(AtomicBool::new(false));
    let is_compacting = Arc::new(AtomicBool::new(false));
    let abort_handle: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
    let bash_state: Arc<Mutex<Option<RunningBash>>> = Arc::new(Mutex::new(None));
    let retry_abort = Arc::new(AtomicBool::new(false));
    let reliability_state = Arc::new(Mutex::new(RpcReliabilityState::new(&options.config)?));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));
    let run_store = RunStore::from_global_dir();

    {
        use futures::future::BoxFuture;

        let steering_state = Arc::clone(&shared_state);
        let follow_state = Arc::clone(&shared_state);
        let steering_cx = cx.clone();
        let follow_cx = cx.clone();
        let mut guard = session
            .lock(&cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        let steering_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
            let steering_state = Arc::clone(&steering_state);
            let steering_cx = steering_cx.clone();
            Box::pin(async move {
                steering_state
                    .lock(&steering_cx)
                    .await
                    .map_or_else(|_| Vec::new(), |mut state| state.pop_steering())
            })
        };
        let follow_fetcher = move || -> BoxFuture<'static, Vec<Message>> {
            let follow_state = Arc::clone(&follow_state);
            let follow_cx = follow_cx.clone();
            Box::pin(async move {
                follow_state
                    .lock(&follow_cx)
                    .await
                    .map_or_else(|_| Vec::new(), |mut state| state.pop_follow_up())
            })
        };
        guard.agent.register_message_fetchers(
            Some(Arc::new(steering_fetcher)),
            Some(Arc::new(follow_fetcher)),
        );
        guard.agent.set_queue_modes(
            options.config.steering_queue_mode(),
            options.config.follow_up_queue_mode(),
        );
    }

    let rpc_extension_manager = {
        let cx_ui = cx.clone();
        let guard = session
            .lock(&cx_ui)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        guard
            .extensions
            .as_ref()
            .map(crate::extensions::ExtensionRegion::manager)
            .cloned()
    };

    let rpc_ui_state: Option<Arc<Mutex<RpcUiBridgeState>>> = rpc_extension_manager
        .as_ref()
        .map(|_| Arc::new(Mutex::new(RpcUiBridgeState::default())));

    if let Some(ref manager) = rpc_extension_manager {
        let (extension_ui_tx, extension_ui_rx) =
            asupersync::channel::mpsc::channel::<ExtensionUiRequest>(64);
        manager.set_ui_sender(extension_ui_tx);

        let out_tx_ui = out_tx.clone();
        let ui_state = rpc_ui_state
            .as_ref()
            .map(Arc::clone)
            .expect("rpc ui state should exist when extension manager exists");
        let manager_ui = (*manager).clone();
        let runtime_handle_ui = options.runtime_handle.clone();
        options.runtime_handle.spawn(async move {
            const MAX_UI_PENDING_REQUESTS: usize = 64;
            let cx = AgentCx::for_request();
            while let Ok(request) = extension_ui_rx.recv(&cx).await {
                if request.expects_response() {
                    let emit_now = {
                        let Ok(mut guard) = ui_state.lock(&cx).await else {
                            return;
                        };
                        if guard.active.is_none() {
                            guard.active = Some(request.clone());
                            true
                        } else if guard.queue.len() < MAX_UI_PENDING_REQUESTS {
                            guard.queue.push_back(request.clone());
                            false
                        } else {
                            drop(guard);
                            let _ = manager_ui.respond_ui(ExtensionUiResponse {
                                id: request.id.clone(),
                                value: None,
                                cancelled: true,
                            });
                            false
                        }
                    };

                    if emit_now {
                        rpc_emit_extension_ui_request(
                            &runtime_handle_ui,
                            Arc::clone(&ui_state),
                            manager_ui.clone(),
                            out_tx_ui.clone(),
                            request,
                        );
                    }
                } else {
                    let rpc_event = request.to_rpc_event();
                    let _ = out_tx_ui.send(event(&rpc_event));
                }
            }
        });
    }

    while let Ok(line) = in_rx.recv(&cx).await {
        if line.trim().is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                let resp = response_error(None, "parse", format!("Failed to parse command: {err}"));
                let _ = out_tx.send(resp);
                continue;
            }
        };

        let Some(command_type_raw) = parsed.get("type").and_then(Value::as_str) else {
            let resp = response_error(None, "parse", "Missing command type".to_string());
            let _ = out_tx.send(resp);
            continue;
        };
        let command_type = normalize_command_type(command_type_raw);

        let id = parsed.get("id").and_then(Value::as_str).map(str::to_string);

        if let Some(response) = rpc_service_commands::handle_service_command(
            &parsed,
            command_type,
            id.clone(),
            RpcServiceCommandContext {
                cx: &cx,
                session: &session,
                reliability_state: &reliability_state,
                orchestration_state: &orchestration_state,
                run_store: &run_store,
                config: &options.config,
            },
        )
        .await
        {
            let _ = out_tx.send(response);
            continue;
        }

        match command_type {
            "prompt"
            | "steer"
            | "follow_up"
            | "abort"
            | "bash"
            | "abort_bash"
            | "extension_ui_response" => {
                if let Some(response) = rpc_transport_commands::handle_transport_command(
                    &parsed,
                    command_type,
                    id,
                    RpcTransportCommandContext {
                        cx: &cx,
                        session: &session,
                        shared_state: &shared_state,
                        options: &options,
                        out_tx: &out_tx,
                        is_streaming: &is_streaming,
                        is_compacting: &is_compacting,
                        abort_handle: &abort_handle,
                        bash_state: &bash_state,
                        retry_abort: &retry_abort,
                        rpc_extension_manager: rpc_extension_manager.as_ref(),
                        rpc_ui_state: rpc_ui_state.as_ref(),
                    },
                )
                .await?
                {
                    let _ = out_tx.send(response);
                }
            }

            "get_state"
            | "get_session_stats"
            | "get_messages"
            | "get_available_models"
            | "set_model"
            | "cycle_model"
            | "set_thinking_level"
            | "cycle_thinking_level"
            | "set_steering_mode"
            | "set_follow_up_mode"
            | "set_auto_compaction"
            | "set_auto_retry"
            | "abort_retry"
            | "set_session_name"
            | "get_last_assistant_text"
            | "export_html"
            | "get_commands" => {
                if let Some(response) = rpc_runtime_commands::handle_runtime_command(
                    &parsed,
                    command_type,
                    id,
                    RpcRuntimeCommandContext {
                        cx: &cx,
                        session: &session,
                        session_handle: &session_handle,
                        shared_state: &shared_state,
                        options: &options,
                        is_streaming: &is_streaming,
                        is_compacting: &is_compacting,
                        retry_abort: &retry_abort,
                    },
                )
                .await?
                {
                    let _ = out_tx.send(response);
                }
            }

            "compact" | "new_session" | "switch_session" | "fork" | "get_fork_messages" => {
                if let Some(response) = rpc_session_commands::handle_session_command(
                    &parsed,
                    command_type,
                    id,
                    RpcSessionCommandContext {
                        cx: &cx,
                        session: &session,
                        shared_state: &shared_state,
                        options: &options,
                        is_compacting: &is_compacting,
                    },
                )
                .await?
                {
                    let _ = out_tx.send(response);
                }
            }

            _ => {
                let _ = out_tx.send(response_error(
                    id,
                    command_type_raw,
                    format!("Unknown command: {command_type_raw}"),
                ));
            }
        }
    }

    let extension_region = session
        .lock(&cx)
        .await
        .ok()
        .and_then(|mut guard| guard.extensions.take());
    if let Some(ext) = extension_region {
        ext.shutdown().await;
    }

    Ok(())
}
