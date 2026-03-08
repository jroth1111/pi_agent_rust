#![allow(clippy::similar_names)]
#![allow(clippy::large_futures)]
#![allow(clippy::too_many_lines)]

//! E2E RPC protocol tests — comprehensive command coverage.
//!
//! These tests drive the RPC server in-process via channels, exercising the full
//! JSON-line protocol for commands that are not yet covered by `rpc_mode.rs` or
//! `rpc_protocol.rs`.

mod common;

use async_trait::async_trait;
use common::TestHarness;
use futures::stream;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::extensions::{ExtensionManager, ExtensionRegion, ExtensionUiRequest};
use pi::http::client::Client;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::models::ModelEntry;
use pi::provider::{InputType, Model, ModelCost, Provider};
use pi::providers::openai::OpenAIProvider;
use pi::resources::ResourceLoader;
use pi::rpc::{RpcOptions, RpcScopedModel, run};
use pi::session::{Session, SessionMessage};
use pi::tools::ToolRegistry;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cassette_root() -> PathBuf {
    std::env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

fn test_model(id: &str, reasoning: bool) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "anthropic".to_string(),
        provider: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        reasoning,
        input: vec![InputType::Text],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        context_window: 200_000,
        max_tokens: 8192,
        headers: HashMap::new(),
    }
}

fn test_entry(id: &str, reasoning: bool) -> ModelEntry {
    ModelEntry {
        model: test_model(id, reasoning),
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

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
        _context: &pi::provider::Context<'_>,
        _options: &pi::provider::StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<pi::model::StreamEvent>> + Send>>,
    > {
        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("noop"))],
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        Ok(Box::pin(stream::iter(vec![Ok(
            pi::model::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        )])))
    }
}

fn build_agent_session_with_provider(
    mut session: Session,
    provider: Arc<dyn Provider>,
    repo_root: &Path,
) -> AgentSession {
    session.header.cwd = repo_root.display().to_string();
    let tools = ToolRegistry::new(&[], repo_root, None);
    let config = AgentConfig::default();
    let agent = Agent::new(provider, tools, config);
    let session = Arc::new(asupersync::sync::Mutex::new(session));
    AgentSession::new(
        agent,
        session,
        false,
        pi::compaction::ResolvedCompactionSettings::default(),
    )
}

fn build_agent_session(session: Session, cassette_dir: &Path) -> AgentSession {
    let model = "gpt-4o-mini".to_string();
    let recorder = VcrRecorder::new_with("e2e_rpc_noop", VcrMode::Playback, cassette_dir);
    let client = Client::new().with_vcr(recorder);
    let provider: Arc<dyn Provider> = Arc::new(OpenAIProvider::new(model).with_client(client));
    build_agent_session_with_provider(session, provider, Path::new(env!("CARGO_MANIFEST_DIR")))
}

fn build_noop_agent_session(session: Session, repo_root: &Path) -> AgentSession {
    build_agent_session_with_provider(session, Arc::new(NoopProvider), repo_root)
}

fn run_git(repo_path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        repo_path.display()
    );
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn setup_inline_repo(prefix: &str) -> (tempfile::TempDir, PathBuf, String) {
    let temp_dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("temp repo");
    let repo_path = temp_dir.path().to_path_buf();
    run_git(&repo_path, &["init"]);
    run_git(&repo_path, &["config", "user.email", "test@example.com"]);
    run_git(&repo_path, &["config", "user.name", "Test User"]);
    std::fs::write(
        repo_path.join("Cargo.toml"),
        "[package]\nname = \"inline-fixture\"\n",
    )
    .expect("write fixture file");
    std::fs::write(repo_path.join("task-a.txt"), "a\n").expect("write task-a fixture");
    std::fs::write(repo_path.join("task-b.txt"), "b\n").expect("write task-b fixture");
    run_git(&repo_path, &["add", "."]);
    run_git(&repo_path, &["commit", "-m", "initial"]);
    let head = git_stdout(&repo_path, &["rev-parse", "HEAD"]);
    (temp_dir, repo_path, head)
}

fn build_options(
    handle: &asupersync::runtime::RuntimeHandle,
    auth_path: PathBuf,
    available_models: Vec<ModelEntry>,
    scoped_models: Vec<RpcScopedModel>,
) -> RpcOptions {
    let auth = AuthStorage::load(auth_path).expect("load auth storage");
    RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models,
        scoped_models,
        auth,
        runtime_handle: handle.clone(),
    }
}

async fn recv_line(rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Result<String, String> {
    let start = Instant::now();
    loop {
        let recv_result = {
            let rx = rx.lock().expect("lock rpc output receiver");
            rx.try_recv()
        };

        match recv_result {
            Ok(line) => return Ok(line),
            Err(TryRecvError::Disconnected) => {
                return Err(format!("{label}: output channel disconnected"));
            }
            Err(TryRecvError::Empty) => {}
        }

        if start.elapsed() > Duration::from_secs(10) {
            return Err(format!("{label}: timed out waiting for output"));
        }

        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

fn parse_response(line: &str) -> Value {
    serde_json::from_str(line.trim()).expect("parse JSON response")
}

/// Send a command and get the response.
async fn send_recv(
    in_tx: &asupersync::channel::mpsc::Sender<String>,
    out_rx: &Arc<Mutex<Receiver<String>>>,
    cmd: &str,
    label: &str,
) -> Value {
    let cx = asupersync::Cx::for_testing();
    in_tx
        .send(&cx, cmd.to_string())
        .await
        .unwrap_or_else(|_| panic!("send {label}"));
    let line = recv_line(out_rx, label)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    parse_response(&line)
}

/// Assert that a response indicates success with the expected command.
fn assert_ok(resp: &Value, command: &str) {
    assert_eq!(resp["type"], "response", "response type for {command}");
    assert_eq!(resp["command"], command);
    assert_eq!(resp["success"], true, "success for {command}: {resp}");
}

/// Assert that a response indicates an error with the expected command.
fn assert_err(resp: &Value, command: &str) {
    assert_eq!(resp["type"], "response", "response type for {command}");
    assert_eq!(resp["command"], command);
    assert_eq!(
        resp["success"], false,
        "expected error for {command}: {resp}"
    );
}

// ---------------------------------------------------------------------------
// Tests: Configuration commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_set_steering_mode_valid() {
    let harness = TestHarness::new("rpc_set_steering_mode_valid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Set to "all"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_steering_mode","mode":"all"}"#,
            "set_steering_mode(all)",
        )
        .await;
        assert_ok(&resp, "set_steering_mode");
        assert_eq!(resp["id"], "1");

        // Set to "one-at-a-time"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_steering_mode","mode":"one-at-a-time"}"#,
            "set_steering_mode(one-at-a-time)",
        )
        .await;
        assert_ok(&resp, "set_steering_mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_steering_mode_invalid() {
    let harness = TestHarness::new("rpc_set_steering_mode_invalid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_steering_mode"}"#,
            "set_steering_mode(missing)",
        )
        .await;
        assert_err(&resp, "set_steering_mode");
        assert_eq!(resp["error"], "Missing mode");

        // Invalid mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_steering_mode","mode":"bogus"}"#,
            "set_steering_mode(bogus)",
        )
        .await;
        assert_err(&resp, "set_steering_mode");
        assert_eq!(resp["error"], "Invalid steering mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_follow_up_mode_valid_and_invalid() {
    let harness = TestHarness::new("rpc_set_follow_up_mode_valid_and_invalid");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Valid
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_follow_up_mode","mode":"all"}"#,
            "set_follow_up_mode(all)",
        )
        .await;
        assert_ok(&resp, "set_follow_up_mode");

        // Missing mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_follow_up_mode"}"#,
            "set_follow_up_mode(missing)",
        )
        .await;
        assert_err(&resp, "set_follow_up_mode");
        assert_eq!(resp["error"], "Missing mode");

        // Invalid mode
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_follow_up_mode","mode":"nope"}"#,
            "set_follow_up_mode(nope)",
        )
        .await;
        assert_err(&resp, "set_follow_up_mode");
        assert_eq!(resp["error"], "Invalid follow-up mode");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_auto_compaction_and_retry() {
    let harness = TestHarness::new("rpc_set_auto_compaction_and_retry");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // set_auto_compaction true
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_auto_compaction","enabled":true}"#,
            "set_auto_compaction(true)",
        )
        .await;
        assert_ok(&resp, "set_auto_compaction");

        // set_auto_compaction missing enabled
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_auto_compaction"}"#,
            "set_auto_compaction(missing)",
        )
        .await;
        assert_err(&resp, "set_auto_compaction");
        assert_eq!(resp["error"], "Missing enabled");

        // set_auto_retry false
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_auto_retry","enabled":false}"#,
            "set_auto_retry(false)",
        )
        .await;
        assert_ok(&resp, "set_auto_retry");

        // set_auto_retry missing enabled
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"4","type":"set_auto_retry"}"#,
            "set_auto_retry(missing)",
        )
        .await;
        assert_err(&resp, "set_auto_retry");
        assert_eq!(resp["error"], "Missing enabled");

        // abort_retry (always succeeds)
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"5","type":"abort_retry"}"#,
            "abort_retry",
        )
        .await;
        assert_ok(&resp, "abort_retry");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Model / Thinking Level commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_available_models_empty() {
    let harness = TestHarness::new("rpc_get_available_models_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_available_models"}"#,
            "get_available_models",
        )
        .await;
        assert_ok(&resp, "get_available_models");
        let models = resp["data"]["models"].as_array().unwrap();
        assert!(models.is_empty(), "expected empty model list");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_available_models_populated() {
    let harness = TestHarness::new("rpc_get_available_models_populated");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let models = vec![
            test_entry("claude-opus-4-6", true),
            test_entry("gpt-4o", false),
        ];
        let options = build_options(&handle, harness.temp_path("auth.json"), models, vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_available_models"}"#,
            "get_available_models",
        )
        .await;
        assert_ok(&resp, "get_available_models");
        let models = resp["data"]["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "claude-opus-4-6");
        assert_eq!(models[0]["reasoning"], true);
        assert_eq!(models[1]["id"], "gpt-4o");
        assert_eq!(models[1]["reasoning"], false);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_orchestration_start_dispatch_and_get_run() {
    let harness = TestHarness::new("rpc_orchestration_start_dispatch_and_get_run");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();
    let run_id = format!("run-e2e-dispatch-{}", uuid::Uuid::new_v4().simple());
    let (repo_guard, repo_path, head) = setup_inline_repo("pi-e2e-wave");

    runtime.block_on(async move {
        let _repo_guard = repo_guard;
        let agent_session = build_noop_agent_session(Session::in_memory(), &repo_path);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let start_cmd = json!({
            "id": "1",
            "type": "orchestration.start_run",
            "runId": run_id,
            "objective": "Exercise orchestration dispatch",
            "tasks": [
                {
                    "taskId": "task-e2e-a",
                    "objective": "Complete task A",
                    "verifyCommand": "true",
                    "inputSnapshot": head,
                    "acceptanceIds": ["ac-a"],
                    "plannedTouches": ["task-a.txt"]
                },
                {
                    "taskId": "task-e2e-b",
                    "objective": "Complete task B",
                    "verifyCommand": "true",
                    "inputSnapshot": head,
                    "acceptanceIds": ["ac-b"],
                    "plannedTouches": ["task-b.txt"]
                }
            ],
            "runVerifyCommand": "test -f task-a.txt && test -f task-b.txt",
            "runVerifyTimeoutSec": 30,
            "maxParallelism": 2
        })
        .to_string();
        let start = send_recv(&in_tx, &out_rx, &start_cmd, "orchestration.start_run").await;
        assert_ok(&start, "orchestration.start_run");
        assert_eq!(start["data"]["run"]["selectedTier"], "wave");

        let dispatch_cmd = json!({
            "id": "2",
            "type": "orchestration.dispatch_run",
            "runId": run_id,
            "agentIdPrefix": "worker",
            "leaseTtlSec": 120
        })
        .to_string();
        let dispatch =
            send_recv(&in_tx, &out_rx, &dispatch_cmd, "orchestration.dispatch_run").await;
        assert_ok(&dispatch, "orchestration.dispatch_run");
        let grants = dispatch["data"]["grants"]
            .as_array()
            .expect("dispatch grants");
        assert!(
            grants.is_empty(),
            "automated wave dispatch should not return live leases"
        );
        assert_eq!(dispatch["data"]["run"]["lifecycle"], "succeeded");

        let get_cmd = json!({
            "id": "3",
            "type": "orchestration.get_run",
            "runId": run_id
        })
        .to_string();
        let get = send_recv(&in_tx, &out_rx, &get_cmd, "orchestration.get_run").await;
        assert_ok(&get, "orchestration.get_run");
        assert_eq!(get["data"]["run"]["runId"], run_id);
        assert_eq!(get["data"]["run"]["taskCounts"]["terminal"], 2);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_orchestration_dispatch_run_executes_inline_run() {
    let harness = TestHarness::new("rpc_orchestration_dispatch_run_executes_inline_run");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();
    let run_id = format!("run-e2e-inline-{}", uuid::Uuid::new_v4().simple());
    let (repo_guard, repo_path, head) = setup_inline_repo("pi-e2e-inline");

    runtime.block_on(async move {
        let _repo_guard = repo_guard;
        let agent_session = build_noop_agent_session(Session::in_memory(), &repo_path);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let start_cmd = json!({
            "id": "1",
            "type": "orchestration.start_run",
            "runId": run_id,
            "objective": "Exercise automated inline orchestration dispatch",
            "tasks": [
                {
                    "taskId": "task-e2e-inline",
                    "objective": "Complete inline task",
                    "verifyCommand": "true",
                    "inputSnapshot": head,
                    "acceptanceIds": ["ac-inline"],
                    "plannedTouches": ["Cargo.toml"]
                }
            ],
            "runVerifyCommand": "true",
            "runVerifyTimeoutSec": 30
        })
        .to_string();
        let start = send_recv(&in_tx, &out_rx, &start_cmd, "orchestration.start_run").await;
        assert_ok(&start, "orchestration.start_run");
        assert_eq!(start["data"]["run"]["selectedTier"], "inline");

        let dispatch_cmd = json!({
            "id": "2",
            "type": "orchestration.dispatch_run",
            "runId": run_id,
            "agentIdPrefix": "worker",
            "leaseTtlSec": 120
        })
        .to_string();
        let dispatch =
            send_recv(&in_tx, &out_rx, &dispatch_cmd, "orchestration.dispatch_run").await;
        assert_ok(&dispatch, "orchestration.dispatch_run");
        let grants = dispatch["data"]["grants"]
            .as_array()
            .expect("dispatch grants");
        assert!(
            grants.is_empty(),
            "inline dispatch should not return live leases"
        );
        assert_eq!(dispatch["data"]["run"]["lifecycle"], "succeeded");
        assert_eq!(dispatch["data"]["run"]["taskCounts"]["terminal"], 1);

        let get_cmd = json!({
            "id": "3",
            "type": "orchestration.get_run",
            "runId": run_id
        })
        .to_string();
        let get = send_recv(&in_tx, &out_rx, &get_cmd, "orchestration.get_run").await;
        assert_ok(&get, "orchestration.get_run");
        assert_eq!(get["data"]["run"]["lifecycle"], "succeeded");
        assert_eq!(get["data"]["run"]["taskCounts"]["terminal"], 1);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_orchestration_resume_reruns_failed_run_verification() {
    let harness = TestHarness::new("rpc_orchestration_resume_reruns_failed_run_verification");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();
    let resume_flag = harness.temp_path("run-verify-ok.flag");
    let resume_flag_str = resume_flag.to_string_lossy().to_string();
    let run_id = format!("run-e2e-resume-{}", uuid::Uuid::new_v4().simple());
    let (repo_guard, repo_path, head) = setup_inline_repo("pi-e2e-resume");

    runtime.block_on(async move {
        let _repo_guard = repo_guard;
        let agent_session = build_noop_agent_session(Session::in_memory(), &repo_path);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let start_cmd = json!({
            "id": "1",
            "type": "orchestration.start_run",
            "runId": run_id,
            "objective": "Exercise run verification resume",
            "tasks": [
                {
                    "taskId": "task-e2e-resume",
                    "objective": "Complete resumable task",
                    "verifyCommand": "true",
                    "inputSnapshot": head,
                    "acceptanceIds": ["ac-resume"],
                    "plannedTouches": ["Cargo.toml"]
                }
            ],
            "runVerifyCommand": format!("test -f {}", resume_flag_str),
            "runVerifyTimeoutSec": 30
        })
        .to_string();
        let start = send_recv(&in_tx, &out_rx, &start_cmd, "orchestration.start_run").await;
        assert_ok(&start, "orchestration.start_run");
        assert_eq!(start["data"]["run"]["selectedTier"], "inline");

        let dispatch_cmd = json!({
            "id": "2",
            "type": "orchestration.dispatch_run",
            "runId": run_id,
            "agentIdPrefix": "worker",
            "leaseTtlSec": 120
        })
        .to_string();
        let dispatch =
            send_recv(&in_tx, &out_rx, &dispatch_cmd, "orchestration.dispatch_run").await;
        assert_ok(&dispatch, "orchestration.dispatch_run");
        let grants = dispatch["data"]["grants"]
            .as_array()
            .expect("dispatch grants");
        assert!(
            grants.is_empty(),
            "inline dispatch should complete in-process without external leases"
        );
        assert_eq!(dispatch["data"]["run"]["lifecycle"], "failed");
        assert_eq!(dispatch["data"]["run"]["latestRunVerify"]["ok"], false);

        let failed_get_cmd = json!({
            "id": "3",
            "type": "orchestration.get_run",
            "runId": run_id
        })
        .to_string();
        let failed_get = send_recv(&in_tx, &out_rx, &failed_get_cmd, "orchestration.get_run").await;
        assert_ok(&failed_get, "orchestration.get_run");
        assert_eq!(failed_get["data"]["run"]["lifecycle"], "failed");
        assert_eq!(failed_get["data"]["run"]["latestRunVerify"]["ok"], false);

        let touch_cmd = json!({
            "id": "4",
            "type": "bash",
            "command": format!("touch {}", resume_flag_str)
        })
        .to_string();
        let touch = send_recv(&in_tx, &out_rx, &touch_cmd, "bash(touch resume flag)").await;
        assert_ok(&touch, "bash");

        let resume_cmd = json!({
            "id": "5",
            "type": "orchestration.resume_run",
            "runId": run_id
        })
        .to_string();
        let resumed = send_recv(&in_tx, &out_rx, &resume_cmd, "orchestration.resume_run").await;
        assert_ok(&resumed, "orchestration.resume_run");
        assert_eq!(resumed["data"]["run"]["lifecycle"], "succeeded");
        assert_eq!(resumed["data"]["run"]["latestRunVerify"]["ok"], true);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_thinking_level_success() {
    let harness = TestHarness::new("rpc_set_thinking_level_success");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Set to high
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level","level":"high"}"#,
            "set_thinking_level(high)",
        )
        .await;
        assert_ok(&resp, "set_thinking_level");

        // Set to off
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_thinking_level","level":"off"}"#,
            "set_thinking_level(off)",
        )
        .await;
        assert_ok(&resp, "set_thinking_level");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_thinking_level_errors() {
    let harness = TestHarness::new("rpc_set_thinking_level_errors");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing level
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_thinking_level"}"#,
            "set_thinking_level(missing)",
        )
        .await;
        assert_err(&resp, "set_thinking_level");
        assert_eq!(resp["error"], "Missing level");

        // Invalid level
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_thinking_level","level":"impossible"}"#,
            "set_thinking_level(impossible)",
        )
        .await;
        assert_err(&resp, "set_thinking_level");
        assert!(
            resp["error"].as_str().is_some_and(|s| !s.is_empty()),
            "expected error message"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Session data commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_messages_empty_session() {
    let harness = TestHarness::new("rpc_get_messages_empty_session");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages(empty)",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let messages = resp["data"]["messages"].as_array().unwrap();
        assert!(messages.is_empty(), "expected empty messages");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_messages_with_user_messages() {
    let harness = TestHarness::new("rpc_get_messages_with_user_messages");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::User {
            content: UserContent::Text("world".to_string()),
            timestamp: Some(now + 1000),
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_messages"}"#,
            "get_messages(with_users)",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let messages = resp["data"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "user");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_last_assistant_text_empty() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text(empty)",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert!(resp["data"]["text"].is_null(), "expected null text");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_last_assistant_text_with_assistant() {
    let harness = TestHarness::new("rpc_get_last_assistant_text_with_assistant");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("Hi there!"))],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: now,
            },
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text(with_assistant)",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert_eq!(resp["data"]["text"], "Hi there!");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_get_commands_empty() {
    let harness = TestHarness::new("rpc_get_commands_empty");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_commands"}"#,
            "get_commands",
        )
        .await;
        assert_ok(&resp, "get_commands");
        assert!(
            resp["data"]["commands"].is_array(),
            "commands should be an array"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Session management commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_set_session_name_success() {
    let harness = TestHarness::new("rpc_set_session_name_success");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name","name":"Test Session"}"#,
            "set_session_name",
        )
        .await;
        assert_ok(&resp, "set_session_name");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_session_name_missing_name() {
    let harness = TestHarness::new("rpc_set_session_name_missing_name");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_session_name"}"#,
            "set_session_name(missing)",
        )
        .await;
        assert_err(&resp, "set_session_name");
        assert_eq!(resp["error"], "Missing name");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Bash command
// ---------------------------------------------------------------------------

#[test]
fn rpc_bash_echo() {
    let harness = TestHarness::new("rpc_bash_echo");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash","command":"echo hello_rpc"}"#,
            "bash(echo)",
        )
        .await;
        assert_ok(&resp, "bash");
        assert_eq!(resp["data"]["exitCode"], 0);
        let output = resp["data"]["output"].as_str().unwrap_or("");
        assert!(
            output.contains("hello_rpc"),
            "bash output should contain hello_rpc, got: {output}"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_bash_missing_command() {
    let harness = TestHarness::new("rpc_bash_missing_command");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash"}"#,
            "bash(missing)",
        )
        .await;
        assert_err(&resp, "bash");
        assert_eq!(resp["error"], "Missing command");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_bash_nonzero_exit() {
    let harness = TestHarness::new("rpc_bash_nonzero_exit");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"bash","command":"exit 42"}"#,
            "bash(exit 42)",
        )
        .await;
        assert_ok(&resp, "bash");
        assert_eq!(resp["data"]["exitCode"], 42);

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Request ID handling
// ---------------------------------------------------------------------------

#[test]
fn rpc_request_id_preserved() {
    let harness = TestHarness::new("rpc_request_id_preserved");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // With string ID
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"custom-id-123","type":"get_state"}"#,
            "get_state(with id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        assert_eq!(resp["id"], "custom-id-123");

        // With numeric ID (RPC server uses as_str(), so numeric IDs are treated as absent)
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":42,"type":"get_state"}"#,
            "get_state(numeric id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        assert!(
            resp.get("id").is_none() || resp["id"].is_null(),
            "numeric IDs should be treated as absent (parsed via as_str)"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_request_without_id() {
    let harness = TestHarness::new("rpc_request_without_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Request without id field
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"type":"get_state"}"#,
            "get_state(no id)",
        )
        .await;
        assert_ok(&resp, "get_state");
        // id should be null or absent
        assert!(
            resp.get("id").is_none() || resp["id"].is_null(),
            "expected no id or null id"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Multiple rapid commands
// ---------------------------------------------------------------------------

#[test]
fn rpc_rapid_sequence_of_sync_commands() {
    let harness = TestHarness::new("rpc_rapid_sequence_of_sync_commands");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let models = vec![test_entry("test-model", false)];
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), models, vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(32);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let cx = asupersync::Cx::for_testing();

        // Fire 8 commands rapidly.
        let commands = [
            (r#"{"id":"1","type":"get_state"}"#, "get_state"),
            (
                r#"{"id":"2","type":"get_available_models"}"#,
                "get_available_models",
            ),
            (r#"{"id":"3","type":"get_messages"}"#, "get_messages"),
            (
                r#"{"id":"4","type":"get_session_stats"}"#,
                "get_session_stats",
            ),
            (r#"{"id":"5","type":"get_commands"}"#, "get_commands"),
            (
                r#"{"id":"6","type":"get_last_assistant_text"}"#,
                "get_last_assistant_text",
            ),
            (
                r#"{"id":"7","type":"set_auto_compaction","enabled":true}"#,
                "set_auto_compaction",
            ),
            (
                r#"{"id":"8","type":"set_auto_retry","enabled":false}"#,
                "set_auto_retry",
            ),
        ];

        for (cmd, _label) in &commands {
            in_tx
                .send(&cx, cmd.to_string())
                .await
                .expect("send rapid command");
        }

        // Collect all 8 responses.
        let mut responses = Vec::new();
        for (_, label) in &commands {
            let line = recv_line(&out_rx, label)
                .await
                .unwrap_or_else(|err| panic!("{err}"));
            responses.push(parse_response(&line));
        }

        // Verify each response matches its command.
        for (i, (_, expected_cmd)) in commands.iter().enumerate() {
            assert_ok(&responses[i], expected_cmd);
            assert_eq!(
                responses[i]["id"],
                serde_json::Value::String((i + 1).to_string())
            );
        }

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: State reflection after mutations
// ---------------------------------------------------------------------------

#[test]
fn rpc_get_state_reflects_session_stats() {
    let harness = TestHarness::new("rpc_get_state_reflects_session_stats");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let now = 1_700_000_000_000i64;
        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-opus-4-6".to_string());
        session.append_message(SessionMessage::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: Some(now),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("world"))],
                api: "test".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-opus-4-6".to_string(),
                usage: Usage {
                    input: 10,
                    output: 5,
                    total_tokens: 15,
                    ..Usage::default()
                },
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: now,
            },
        });

        let agent_session = build_agent_session(session, &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // get_session_stats should reflect pre-populated messages
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"get_session_stats"}"#,
            "get_session_stats",
        )
        .await;
        assert_ok(&resp, "get_session_stats");
        assert_eq!(resp["data"]["userMessages"], 1);
        assert_eq!(resp["data"]["assistantMessages"], 1);
        assert_eq!(resp["data"]["totalMessages"], 2);
        assert_eq!(resp["data"]["tokens"]["input"], 10);
        assert_eq!(resp["data"]["tokens"]["output"], 5);
        assert_eq!(resp["data"]["tokens"]["total"], 15);

        // get_messages should return the 2 messages
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"get_messages"}"#,
            "get_messages",
        )
        .await;
        assert_ok(&resp, "get_messages");
        let msgs = resp["data"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");

        // get_last_assistant_text should return "world"
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"get_last_assistant_text"}"#,
            "get_last_assistant_text",
        )
        .await;
        assert_ok(&resp, "get_last_assistant_text");
        assert_eq!(resp["data"]["text"], "world");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: Error path coverage
// ---------------------------------------------------------------------------

#[test]
fn rpc_prompt_missing_message() {
    let harness = TestHarness::new("rpc_prompt_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"prompt"}"#,
            "prompt(missing message)",
        )
        .await;
        assert_err(&resp, "prompt");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_steer_missing_message() {
    let harness = TestHarness::new("rpc_steer_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"steer"}"#,
            "steer(missing message)",
        )
        .await;
        assert_err(&resp, "steer");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_follow_up_missing_message() {
    let harness = TestHarness::new("rpc_follow_up_missing_message");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"follow_up"}"#,
            "follow_up(missing message)",
        )
        .await;
        assert_err(&resp, "follow_up");
        assert_eq!(resp["error"], "Missing message");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_set_model_missing_model_id() {
    let harness = TestHarness::new("rpc_set_model_missing_model_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Missing provider
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"set_model","modelId":"x"}"#,
            "set_model(missing provider)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert_eq!(resp["error"], "Missing provider");

        // Missing modelId
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"set_model","provider":"anthropic"}"#,
            "set_model(missing modelId)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert_eq!(resp["error"], "Missing modelId");

        // Model not found
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"3","type":"set_model","provider":"anthropic","modelId":"nonexistent"}"#,
            "set_model(not found)",
        )
        .await;
        assert_err(&resp, "set_model");
        assert!(
            resp["error"]
                .as_str()
                .is_some_and(|s| s.contains("Model not found")),
            "expected model not found error: {resp}"
        );

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_fork_missing_entry_id() {
    let harness = TestHarness::new("rpc_fork_missing_entry_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"fork"}"#,
            "fork(missing entryId)",
        )
        .await;
        assert_err(&resp, "fork");
        assert_eq!(resp["error"], "Missing entryId");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_export_html_empty_session() {
    let harness = TestHarness::new("rpc_export_html_empty_session");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        let output = harness.temp_path("export.html");
        let cmd = serde_json::json!({
            "id": "1",
            "type": "export_html",
            "outputPath": output.display().to_string()
        })
        .to_string();
        let resp = send_recv(&in_tx, &out_rx, &cmd, "export_html").await;
        assert_ok(&resp, "export_html");
        assert!(resp["data"]["path"].is_string(), "should return path");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Tests: abort (when not streaming)
// ---------------------------------------------------------------------------

#[test]
fn rpc_abort_when_idle() {
    let harness = TestHarness::new("rpc_abort_when_idle");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(&handle, harness.temp_path("auth.json"), vec![], vec![]);
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Abort when nothing is streaming should still succeed.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"1","type":"abort"}"#,
            "abort(idle)",
        )
        .await;
        assert_ok(&resp, "abort");

        // abort_bash when nothing is running should also succeed.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"2","type":"abort_bash"}"#,
            "abort_bash(idle)",
        )
        .await;
        assert_ok(&resp, "abort_bash");

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

// ---------------------------------------------------------------------------
// Helpers: Extension UI roundtrip
// ---------------------------------------------------------------------------

/// Build an `AgentSession` with an `ExtensionManager` attached so the RPC
/// server sets up the extension UI channel.  Returns both the session (to
/// hand to `run()`) and a cloned `ExtensionManager` the test can use to
/// call `request_ui()` / `respond_ui()`.
fn build_agent_session_with_extensions(
    session: Session,
    cassette_dir: &Path,
) -> (AgentSession, ExtensionManager) {
    let manager = ExtensionManager::default();
    let region = ExtensionRegion::new(manager.clone());
    let mut agent_session = build_agent_session(session, cassette_dir);
    agent_session.extensions = Some(region);
    (agent_session, manager)
}

/// Wait for an `extension_ui_request` event on the RPC output channel.
/// Skips any non-event / non-ui-request lines.
async fn recv_ui_request(out_rx: &Arc<Mutex<Receiver<String>>>, label: &str) -> Value {
    let start = Instant::now();
    loop {
        let recv_result = {
            let rx = out_rx.lock().expect("lock rpc output receiver");
            rx.try_recv()
        };

        match recv_result {
            Ok(line) => {
                if let Ok(val) = serde_json::from_str::<Value>(&line) {
                    if val.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
                        return val;
                    }
                }
                // Not our event — keep waiting.
            }
            Err(TryRecvError::Disconnected) => {
                panic!(
                    "{label}: output channel disconnected while waiting for extension_ui_request"
                );
            }
            Err(TryRecvError::Empty) => {}
        }

        assert!(
            start.elapsed() <= Duration::from_secs(10),
            "{label}: timed out waiting for extension_ui_request"
        );
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — confirm
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_confirm_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_confirm_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_confirm.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Give the server a moment to set up the UI channel.
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        // Spawn a request_ui call from the extension side.
        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-confirm-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({
                    "title": "Delete file?",
                    "message": "This cannot be undone.",
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: Some("test-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        // Capture the emitted extension_ui_request event.
        let ui_event = recv_ui_request(&out_rx, "confirm").await;
        assert_eq!(ui_event["type"], "extension_ui_request");
        assert_eq!(ui_event["id"], "req-confirm-1");
        assert_eq!(ui_event["method"], "confirm");
        assert_eq!(ui_event["title"], "Delete file?");

        // Respond with confirmed = true.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-1","type":"extension_ui_response","requestId":"req-confirm-1","confirmed":true}"#,
            "confirm_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");
        assert_eq!(resp["data"]["resolved"], true);

        // Verify the request_ui future resolved with the correct value.
        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!(true)));

        drop(in_tx);
        let result = server.await;
        assert!(result.is_ok(), "rpc server error: {result:?}");
    });
}

#[test]
fn rpc_extension_ui_confirm_denied() {
    let _harness = TestHarness::new("rpc_extension_ui_confirm_denied");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_deny.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-deny-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Do risky thing?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "confirm_denied").await;
        assert_eq!(ui_event["id"], "req-deny-1");

        // Respond with confirmed = false.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-2","type":"extension_ui_response","requestId":"req-deny-1","value":false}"#,
            "confirm_denied_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!(false)));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — select
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_select_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_select_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_select.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-select-1".to_string(),
                method: "select".to_string(),
                payload: json!({
                    "title": "Pick a model",
                    "options": ["claude-sonnet", "gpt-4o", "gemini-pro"],
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: Some("model-picker-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "select").await;
        assert_eq!(ui_event["id"], "req-select-1");
        assert_eq!(ui_event["method"], "select");
        assert_eq!(ui_event["title"], "Pick a model");
        assert!(ui_event["options"].is_array());

        // Select the second option.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-3","type":"extension_ui_response","requestId":"req-select-1","value":"gpt-4o"}"#,
            "select_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!("gpt-4o")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — input
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_input_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_input_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_input.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-input-1".to_string(),
                method: "input".to_string(),
                payload: json!({
                    "title": "Enter API key",
                    "message": "Paste your key below",
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: Some("api-key-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "input").await;
        assert_eq!(ui_event["id"], "req-input-1");
        assert_eq!(ui_event["method"], "input");

        // Respond with typed text.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-4","type":"extension_ui_response","requestId":"req-input-1","value":"sk-test-12345"}"#,
            "input_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!("sk-test-12345")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI roundtrip — editor
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_editor_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_editor_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_editor.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-editor-1".to_string(),
                method: "editor".to_string(),
                payload: json!({
                    "title": "Edit config",
                    "message": "Modify the YAML below",
                    "timeout": 5000
                }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "editor").await;
        assert_eq!(ui_event["id"], "req-editor-1");
        assert_eq!(ui_event["method"], "editor");

        // Respond with edited text.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-5","type":"extension_ui_response","requestId":"req-editor-1","value":"key: new_value"}"#,
            "editor_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(!response.cancelled);
        assert_eq!(response.value, Some(json!("key: new_value")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — cancellation
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_cancel_response() {
    let _harness = TestHarness::new("rpc_extension_ui_cancel_response");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_cancel.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-cancel-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Proceed?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "cancel").await;
        assert_eq!(ui_event["id"], "req-cancel-1");

        // Respond with cancelled: true.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-6","type":"extension_ui_response","requestId":"req-cancel-1","cancelled":true}"#,
            "cancel_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert!(response.cancelled);

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — no extensions configured (noop fallback)
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_response_without_extensions() {
    let _harness = TestHarness::new("rpc_extension_ui_response_without_extensions");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        // No extensions — build_agent_session leaves extensions = None.
        let agent_session = build_agent_session(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_noext.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Sending extension_ui_response when no extensions are configured
        // should return a success noop (no data).
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-7","type":"extension_ui_response","requestId":"req-x","confirmed":true}"#,
            "no_extensions",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");
        assert!(resp.get("data").is_none() || resp["data"].is_null());

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — mismatched request ID
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_mismatched_request_id() {
    let _harness = TestHarness::new("rpc_extension_ui_mismatched_request_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_mismatch.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let _ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-real-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Do it?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "mismatch").await;
        assert_eq!(ui_event["id"], "req-real-1");

        // Send response with WRONG request ID.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-8","type":"extension_ui_response","requestId":"req-WRONG","confirmed":true}"#,
            "wrong_id_response",
        )
        .await;
        assert_err(&resp, "extension_ui_response");
        let error_msg = resp["error"].as_str().unwrap_or("");
        assert!(
            error_msg.contains("Unexpected requestId"),
            "error should mention unexpected requestId: {error_msg}"
        );

        // Now send the correct one to clean up.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-9","type":"extension_ui_response","requestId":"req-real-1","confirmed":true}"#,
            "correct_id_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — missing requestId
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_missing_request_id() {
    let _harness = TestHarness::new("rpc_extension_ui_missing_request_id");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, _manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_noid.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Send extension_ui_response without requestId OR id — the parser
        // falls back to "id" as an alias, so we must omit both to trigger
        // the "Missing requestId" error.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"type":"extension_ui_response","confirmed":true}"#,
            "missing_id",
        )
        .await;
        assert_err(&resp, "extension_ui_response");
        let error_msg = resp["error"].as_str().unwrap_or("");
        assert!(
            error_msg.contains("Missing requestId"),
            "error should mention missing requestId: {error_msg}"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — legacy id alias accepted
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_id_alias_roundtrip() {
    let _harness = TestHarness::new("rpc_extension_ui_id_alias_roundtrip");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_id_alias.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-legacy-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "Legacy id alias?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: None,
            };
            mgr.request_ui(request).await
        });

        let ui_event = recv_ui_request(&out_rx, "id_alias").await;
        assert_eq!(ui_event["id"], "req-legacy-1");
        assert_eq!(ui_event["method"], "confirm");

        // Upstream accepts top-level "id" as a requestId alias for
        // extension_ui_response.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"req-legacy-1","type":"extension_ui_response","confirmed":true}"#,
            "id_alias_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");
        assert_eq!(resp["id"], "req-legacy-1");
        assert_eq!(resp["data"]["resolved"], true);

        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        let response = response.expect("should have a response");
        assert_eq!(response.value, Some(json!(true)));
        assert!(!response.cancelled);

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — sequential ordering (one at a time)
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_sequential_ordering() {
    let _harness = TestHarness::new("rpc_extension_ui_sequential_ordering");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_seq.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        // Fire two requests concurrently.
        let mgr1 = manager.clone();
        let ui_task_1 = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-seq-1".to_string(),
                method: "confirm".to_string(),
                payload: json!({ "title": "First?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: Some("ext-a".to_string()),
            };
            mgr1.request_ui(request).await
        });

        // Wait for the first to be emitted before sending the second.
        let first_event = recv_ui_request(&out_rx, "seq_first").await;
        assert_eq!(first_event["id"], "req-seq-1");

        let mgr2 = manager.clone();
        let ui_task_2 = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-seq-2".to_string(),
                method: "input".to_string(),
                payload: json!({ "title": "Second?", "timeout": 5000 }),
                timeout_ms: Some(5000),
                extension_id: Some("ext-b".to_string()),
            };
            mgr2.request_ui(request).await
        });

        // Give the second request time to enter the queue.
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(100)).await;

        // Respond to the first — this should dequeue the second.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-11","type":"extension_ui_response","requestId":"req-seq-1","confirmed":true}"#,
            "seq_first_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let r1 = ui_task_1
            .await
            .expect("first request_ui")
            .expect("has response");
        assert_eq!(r1.value, Some(json!(true)));

        // Now the second request should be emitted.
        let second_event = recv_ui_request(&out_rx, "seq_second").await;
        assert_eq!(second_event["id"], "req-seq-2");
        assert_eq!(second_event["method"], "input");

        // Respond to the second.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-12","type":"extension_ui_response","requestId":"req-seq-2","value":"hello"}"#,
            "seq_second_response",
        )
        .await;
        assert_ok(&resp, "extension_ui_response");

        let r2 = ui_task_2
            .await
            .expect("second request_ui")
            .expect("has response");
        assert_eq!(r2.value, Some(json!("hello")));

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — no active request error
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_no_active_request() {
    let _harness = TestHarness::new("rpc_extension_ui_no_active_request");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, _manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_noactive.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });

        // Send a response when no UI request is active.
        let resp = send_recv(
            &in_tx,
            &out_rx,
            r#"{"id":"cmd-13","type":"extension_ui_response","requestId":"req-ghost","confirmed":true}"#,
            "no_active",
        )
        .await;
        assert_err(&resp, "extension_ui_response");
        let error_msg = resp["error"].as_str().unwrap_or("");
        assert!(
            error_msg.contains("No active extension UI request"),
            "error should mention no active request: {error_msg}"
        );

        drop(in_tx);
        let _ = server.await;
    });
}

// ---------------------------------------------------------------------------
// Tests: Extension UI — fire-and-forget (notify) emitted but not queued
// ---------------------------------------------------------------------------

#[test]
fn rpc_extension_ui_notify_fire_and_forget() {
    let _harness = TestHarness::new("rpc_extension_ui_notify_fire_and_forget");
    let cassette_dir = cassette_root();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (agent_session, manager) =
            build_agent_session_with_extensions(Session::in_memory(), &cassette_dir);
        let options = build_options(
            &handle,
            PathBuf::from("/tmp/auth_ui_notify.json"),
            vec![],
            vec![],
        );
        let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(16);
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
        let out_rx = Arc::new(Mutex::new(out_rx));

        let server = handle.spawn(async move { run(agent_session, options, in_rx, out_tx).await });
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

        // Send a "notify" request — fire-and-forget, no response expected.
        let mgr = manager.clone();
        let ui_task = handle.spawn(async move {
            let request = ExtensionUiRequest {
                id: "req-notify-1".to_string(),
                method: "notify".to_string(),
                payload: json!({
                    "title": "Heads up!",
                    "message": "Something happened"
                }),
                timeout_ms: None,
                extension_id: Some("notifier-ext".to_string()),
            };
            mgr.request_ui(request).await
        });

        // The event should still be emitted to the RPC output.
        let ui_event = recv_ui_request(&out_rx, "notify").await;
        assert_eq!(ui_event["type"], "extension_ui_request");
        assert_eq!(ui_event["id"], "req-notify-1");
        assert_eq!(ui_event["method"], "notify");

        // request_ui should return Ok(None) for fire-and-forget.
        let ui_result = ui_task.await;
        let response = ui_result.expect("request_ui should succeed");
        assert!(response.is_none(), "notify should not return a response");

        drop(in_tx);
        let _ = server.await;
    });
}
