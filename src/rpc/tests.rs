use super::test_support::run_service_test_helpers::{
    append_dispatch_grants_session_entries, build_runtime_task_report, build_submit_task_report,
    cancel_live_run_tasks, cancel_live_run_tasks_and_sync, completed_run_verify_scope,
    current_run_status, dispatch_run_wave, execute_run_verification, persist_run_status,
    refresh_run_from_reliability, refresh_task_runs,
};
use super::test_support::{ensure_run_id_available, select_execution_tier};
use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession, QueueMode};
use crate::auth::AuthCredential;
use crate::compaction::ResolvedCompactionSettings;
use crate::config::{ReliabilityConfig, parse_queue_mode};
use crate::extensions::ExtensionUiRequest;
use crate::model::{
    AssistantMessage, ContentBlock, ImageContent, Message, StopReason, StreamEvent, TextContent,
    ThinkingLevel, Usage, UserContent, UserMessage,
};
use crate::orchestration::RunVerifyScopeKind;
use crate::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use crate::services::run_service::{
    OrchestrationInlineWorker, dispatch_workspace_segment_id, execute_dispatch_grants_with_worker,
    execute_inline_dispatch_grant_with_worker,
};
use crate::session::Session;
use crate::surface::rpc_server::try_send_line_with_backpressure;
use crate::surface::rpc_support::session_state;
use crate::surface::rpc_support::{extract_user_text, rpc_flatten_content_blocks};
use crate::tools::ToolRegistry;
use asupersync::channel::mpsc;
use async_trait::async_trait;
use futures::stream;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::time::Instant;

// -----------------------------------------------------------------------
// Helper builders
// -----------------------------------------------------------------------

fn dummy_model(id: &str, reasoning: bool) -> Model {
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

fn dummy_entry(id: &str, reasoning: bool) -> ModelEntry {
    ModelEntry {
        model: dummy_model(id, reasoning),
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn rpc_options_with_models(available_models: Vec<ModelEntry>) -> RpcOptions {
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .blocking_threads(1, 1)
        .build()
        .expect("runtime build");
    let runtime_handle = runtime.handle();

    let auth_path = tempfile::tempdir()
        .expect("tempdir")
        .path()
        .join("auth.json");
    let auth = AuthStorage::load(auth_path).expect("auth load");

    RpcOptions {
        config: Config::default(),
        resources: ResourceLoader::empty(false),
        available_models,
        scoped_models: Vec::new(),
        auth,
        runtime_handle,
    }
}

#[derive(Debug)]
struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CountingProvider {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn api(&self) -> &'static str {
        "counting"
    }

    fn model_id(&self) -> &'static str {
        "counting-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> crate::error::Result<
        Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
    > {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "reply-{call_index}"
            )))],
            api: "counting".to_string(),
            provider: "counting".to_string(),
            model: "counting-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 1_700_000_000,
        };
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamEvent::Start {
                partial: message.clone(),
            }),
            Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            }),
        ])))
    }
}

fn queued_user_message(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 1_700_000_000,
    })
}

async fn register_rpc_queue_fetchers_for_test(
    session: &Arc<Mutex<AgentSession>>,
    shared_state: &Arc<Mutex<RpcSharedState>>,
    cx: &AgentCx,
) {
    use futures::future::BoxFuture;

    let steering_state = Arc::clone(shared_state);
    let follow_state = Arc::clone(shared_state);
    let steering_cx = cx.clone();
    let follow_cx = cx.clone();

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

    let mut guard = session.lock(cx).await.expect("session lock");
    guard.agent.register_message_fetchers(
        Some(Arc::new(steering_fetcher)),
        Some(Arc::new(follow_fetcher)),
    );
}

fn reliability_state_for_tests(
    mode: ReliabilityEnforcementMode,
    require_evidence_for_close: bool,
    allow_open_ended_defer: bool,
) -> RpcReliabilityState {
    let config = Config {
        reliability: Some(ReliabilityConfig {
            enabled: Some(true),
            enforcement_mode: Some(mode),
            require_evidence_for_close: Some(require_evidence_for_close),
            max_touched_files: Some(8),
            default_max_attempts: Some(3),
            allow_open_ended_defer: Some(allow_open_ended_defer),
            verify_timeout_sec_default: Some(60),
            lease_provider: None,
        }),
        ..Config::default()
    };
    RpcReliabilityState::new(&config).expect("reliability state")
}

fn reliability_contract(task_id: &str, prerequisites: Vec<TaskPrerequisite>) -> TaskContract {
    TaskContract {
        task_id: task_id.to_string(),
        objective: format!("Objective {task_id}"),
        parent_goal_trace_id: Some(format!("goal:{task_id}")),
        invariants: Vec::new(),
        max_touched_files: None,
        forbid_paths: Vec::new(),
        verify_command: "cargo test".to_string(),
        verify_timeout_sec: Some(30),
        max_attempts: Some(3),
        input_snapshot: Some("snapshot".to_string()),
        acceptance_ids: vec!["ac-1".to_string()],
        planned_touches: vec![format!("src/{task_id}.rs")],
        prerequisites,
        enforce_symbol_drift_check: false,
    }
}

fn run_async<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build test runtime");
    runtime.block_on(future)
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

fn git_stdout(repo_path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        repo_path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn setup_inline_execution_repo() -> (tempfile::TempDir, PathBuf, String) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let repo_path = temp_dir.path().to_path_buf();
    run_git(&repo_path, &["init"]);
    run_git(&repo_path, &["config", "user.email", "test@example.com"]);
    run_git(&repo_path, &["config", "user.name", "Test User"]);
    fs::create_dir_all(repo_path.join("src")).expect("mkdir src");
    fs::write(
        repo_path.join("src/lib.rs"),
        "pub fn fixture() { println!(\"before\"); }\n",
    )
    .expect("write seed file");
    run_git(&repo_path, &["add", "."]);
    run_git(&repo_path, &["commit", "-m", "initial"]);
    let head = git_stdout(&repo_path, &["rev-parse", "HEAD"]);
    (temp_dir, repo_path, head)
}

#[derive(Debug)]
struct NoopProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for NoopProvider {
    fn name(&self) -> &'static str {
        "test-provider"
    }

    fn api(&self) -> &'static str {
        "test-api"
    }

    fn model_id(&self) -> &'static str {
        "test-model"
    }

    async fn stream(
        &self,
        _context: &crate::provider::Context<'_>,
        _options: &crate::provider::StreamOptions,
    ) -> crate::error::Result<
        Pin<
            Box<dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>> + Send>,
        >,
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
            crate::model::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        )])))
    }
}

fn build_test_agent_session(repo_path: &Path) -> Arc<Mutex<AgentSession>> {
    let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
    let tools = crate::tools::ToolRegistry::new(&[], repo_path, None);
    let agent = Agent::new(provider, tools, AgentConfig::default());
    let session = Arc::new(Mutex::new(Session::in_memory()));
    run_async(async {
        let mut inner = session
            .lock(&AgentCx::for_request())
            .await
            .expect("lock session");
        inner.header.cwd = repo_path.display().to_string();
    });
    Arc::new(Mutex::new(AgentSession::new(
        agent,
        session,
        false,
        ResolvedCompactionSettings::default(),
    )))
}

struct FixtureInlineWorker {
    target: String,
    contents: String,
    summary: String,
}

#[async_trait]
impl OrchestrationInlineWorker for FixtureInlineWorker {
    async fn execute(&self, workspace_path: &Path, _task: &TaskContract) -> Result<String> {
        let target = workspace_path.join(&self.target);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| Error::Io(Box::new(err)))?;
        }
        fs::write(&target, &self.contents).map_err(|err| Error::Io(Box::new(err)))?;
        Ok(self.summary.clone())
    }
}

struct ConcurrentFixtureInlineWorker {
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    sleep: Duration,
}

#[async_trait]
impl OrchestrationInlineWorker for ConcurrentFixtureInlineWorker {
    async fn execute(&self, workspace_path: &Path, task: &TaskContract) -> Result<String> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);

        let result = async {
            asupersync::time::sleep(asupersync::time::wall_now(), self.sleep).await;
            let target_rel = task
                .planned_touches
                .first()
                .ok_or_else(|| Error::validation("missing planned touches"))?;
            let target = workspace_path.join(target_rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|err| Error::Io(Box::new(err)))?;
            }
            fs::write(
                &target,
                format!("pub fn {}() {{}}\n", task.task_id.replace('-', "_")),
            )
            .map_err(|err| Error::Io(Box::new(err)))?;
            Ok(format!("Updated {}", task.task_id))
        }
        .await;

        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

struct OverlapReplayFixtureWorker {
    target: String,
}

#[async_trait]
impl OrchestrationInlineWorker for OverlapReplayFixtureWorker {
    async fn execute(&self, workspace_path: &Path, task: &TaskContract) -> Result<String> {
        let target = workspace_path.join(&self.target);
        let mut contents = fs::read_to_string(&target).map_err(|err| Error::Io(Box::new(err)))?;

        match task.task_id.as_str() {
            "task-overlap-a" => contents.push_str("// alpha\n"),
            "task-overlap-b" => {
                if contents.contains("alpha") {
                    contents.push_str("// beta-after-alpha\n");
                } else {
                    contents.push_str("// beta-alone\n");
                }
            }
            other => {
                return Err(Error::validation(format!(
                    "unexpected overlap replay task: {other}"
                )));
            }
        }

        fs::write(&target, contents).map_err(|err| Error::Io(Box::new(err)))?;
        Ok(format!("Updated {}", task.task_id))
    }
}

struct FailingInlineWorker {
    message: String,
}

#[async_trait]
impl OrchestrationInlineWorker for FailingInlineWorker {
    async fn execute(&self, _workspace_path: &Path, _task: &TaskContract) -> Result<String> {
        Err(Error::validation(self.message.clone()))
    }
}

struct PartialWaveFailureWorker {
    failing_task_id: String,
}

#[async_trait]
impl OrchestrationInlineWorker for PartialWaveFailureWorker {
    async fn execute(&self, workspace_path: &Path, task: &TaskContract) -> Result<String> {
        if task.task_id == self.failing_task_id {
            return Err(Error::validation(format!(
                "fixture partial wave failure for {}",
                task.task_id
            )));
        }

        let target_rel = task
            .planned_touches
            .first()
            .ok_or_else(|| Error::validation("missing planned touches"))?;
        let target = workspace_path.join(target_rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| Error::Io(Box::new(err)))?;
        }
        fs::write(
            &target,
            format!("pub fn {}() {{}}\n", task.task_id.replace('-', "_")),
        )
        .map_err(|err| Error::Io(Box::new(err)))?;
        Ok(format!("Updated {}", task.task_id))
    }
}

#[test]
fn reliability_external_lease_provider_conflicts() {
    let provider = Arc::new(reliability::InMemoryExternalLeaseProvider::default());

    let mut primary = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    primary.set_external_lease_provider(provider.clone());

    let mut secondary = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    secondary.set_external_lease_provider(provider.clone());

    let contract = reliability_contract("task-external-lease", Vec::new());
    let lease_a = primary
        .request_dispatch(&contract, "agent-a", 300)
        .expect("primary dispatch");

    let conflict = secondary
        .request_dispatch(&contract, "agent-b", 300)
        .expect_err("secondary dispatch should conflict on external provider");
    assert!(
        conflict.to_string().contains("already leased"),
        "unexpected conflict error: {conflict}"
    );

    provider.force_expire_for_test(
        &lease_a.lease_id,
        chrono::Utc::now() - chrono::Duration::seconds(1),
    );

    let lease_b = secondary
        .request_dispatch(&contract, "agent-b", 300)
        .expect("stale external lease should be recoverable");
    assert_eq!(lease_b.task_id, "task-external-lease");
    assert_eq!(lease_b.state, "leased");
}

#[test]
fn orchestration_tier_selection_prefers_inline_for_single_task() {
    let tasks = vec![reliability_contract("task-inline", Vec::new())];
    assert_eq!(select_execution_tier(&tasks), ExecutionTier::Inline);
}

#[test]
fn orchestration_tier_selection_prefers_wave_for_small_shallow_graphs() {
    let tasks = vec![
        reliability_contract("task-a", Vec::new()),
        reliability_contract(
            "task-b",
            vec![TaskPrerequisite {
                task_id: "task-a".to_string(),
                trigger: reliability::EdgeTrigger::OnSuccess,
            }],
        ),
    ];

    assert_eq!(select_execution_tier(&tasks), ExecutionTier::Wave);
}

#[test]
fn orchestration_tier_selection_prefers_hierarchical_for_deep_graphs() {
    let mut tasks = Vec::new();
    let mut previous: Option<String> = None;
    for index in 0..5 {
        let task_id = format!("task-{index}");
        let prerequisites: Vec<_> = previous
            .iter()
            .map(|prior| TaskPrerequisite {
                task_id: prior.clone(),
                trigger: reliability::EdgeTrigger::OnSuccess,
            })
            .collect();
        tasks.push(reliability_contract(&task_id, prerequisites));
        previous = Some(task_id);
    }

    assert_eq!(select_execution_tier(&tasks), ExecutionTier::Hierarchical);
}

#[test]
fn orchestration_state_tracks_task_membership_updates() {
    let mut orchestration = RpcOrchestrationState::default();
    let mut run = RunStatus::new("run-1", "Ship it", ExecutionTier::Wave);
    run.task_ids = vec!["task-a".to_string(), "task-b".to_string()];
    orchestration.register_run(run.clone());

    assert_eq!(
        orchestration.run_ids_for_task("task-a"),
        vec!["run-1".to_string()]
    );
    assert_eq!(
        orchestration.run_ids_for_task("task-b"),
        vec!["run-1".to_string()]
    );

    run.task_ids = vec!["task-b".to_string()];
    orchestration.update_run(run);

    assert!(orchestration.run_ids_for_task("task-a").is_empty());
    assert_eq!(
        orchestration.run_ids_for_task("task-b"),
        vec!["run-1".to_string()]
    );
}

#[test]
fn orchestration_run_id_availability_rejects_cached_and_persisted_runs() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let store = RunStore::new(temp_dir.path().to_path_buf());
    let mut orchestration = RpcOrchestrationState::default();
    let cached_run = RunStatus::new("run-cached", "Cached", ExecutionTier::Inline);
    orchestration.register_run(cached_run);

    let persisted_run = RunStatus::new("run-persisted", "Persisted", ExecutionTier::Inline);
    store.save(&persisted_run).expect("save persisted run");

    let cached_err = ensure_run_id_available(&orchestration, &store, "run-cached")
        .expect_err("cached run should conflict");
    assert!(cached_err.to_string().contains("already exists"));

    let persisted_err = ensure_run_id_available(&orchestration, &store, "run-persisted")
        .expect_err("persisted run should conflict");
    assert!(persisted_err.to_string().contains("already exists"));

    assert!(ensure_run_id_available(&orchestration, &store, "run-new").is_ok());
}

#[test]
fn orchestration_refresh_run_tracks_active_wave_for_running_tasks() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract_a = reliability_contract("task-a", Vec::new());
    let contract_b = reliability_contract("task-b", Vec::new());

    reliability
        .get_or_create_task(&contract_a)
        .expect("register task a");
    reliability
        .get_or_create_task(&contract_b)
        .expect("register task b");
    reliability
        .request_dispatch(&contract_a, "agent-1", 60)
        .expect("dispatch task a");

    let mut run = RunStatus::new("run-wave", "Wave", ExecutionTier::Wave);
    run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Running);
    assert_eq!(run.task_counts.get("leased"), Some(&1));
    assert_eq!(run.task_counts.get("ready"), Some(&1));
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        vec!["task-a".to_string()]
    );
}

#[test]
fn orchestration_refresh_run_plans_disjoint_ready_wave() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract_a = reliability_contract("task-a", Vec::new());
    let contract_b = reliability_contract("task-b", Vec::new());

    reliability
        .get_or_create_task(&contract_a)
        .expect("register task a");
    reliability
        .get_or_create_task(&contract_b)
        .expect("register task b");

    let mut run = RunStatus::new("run-ready", "Ready wave", ExecutionTier::Wave);
    run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Pending);
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        vec!["task-a".to_string(), "task-b".to_string()]
    );
}

#[test]
fn orchestration_refresh_run_treats_future_recoverable_as_pending() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract = reliability_contract("task-recoverable", Vec::new());

    reliability
        .get_or_create_task(&contract)
        .expect("register recoverable task");
    let task = reliability
        .tasks
        .get_mut(&contract.task_id)
        .expect("recoverable task exists");
    task.runtime.state = reliability::RuntimeState::Recoverable {
        reason: reliability::FailureClass::VerificationFailed,
        failure_artifact: None,
        handoff_summary: "retry later".to_string(),
        retry_after: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
    };

    let mut run = RunStatus::new("run-recoverable", "Recoverable wait", ExecutionTier::Wave);
    run.task_ids = vec![contract.task_id.clone()];
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Pending);
    assert_eq!(run.task_counts.get("recoverable"), Some(&1));
    assert!(run.active_wave.is_none());
}

#[test]
fn orchestration_refresh_run_keeps_in_flight_over_recoverable() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract_running = reliability_contract("task-running", Vec::new());
    let contract_recoverable = reliability_contract("task-recoverable", Vec::new());

    reliability
        .get_or_create_task(&contract_running)
        .expect("register running task");
    reliability
        .get_or_create_task(&contract_recoverable)
        .expect("register recoverable task");

    let expires_at = chrono::Utc::now() + chrono::Duration::minutes(5);
    reliability
        .tasks
        .get_mut("task-running")
        .expect("running task")
        .runtime
        .state = reliability::RuntimeState::Leased {
        lease_id: "lease-running".to_string(),
        agent_id: "agent-running".to_string(),
        fence_token: 1,
        expires_at,
    };
    reliability
        .tasks
        .get_mut("task-recoverable")
        .expect("recoverable task")
        .runtime
        .state = reliability::RuntimeState::Recoverable {
        reason: reliability::FailureClass::VerificationFailed,
        failure_artifact: None,
        handoff_summary: "retry later".to_string(),
        retry_after: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
    };

    let mut run = RunStatus::new("run-mixed", "Mixed run", ExecutionTier::Wave);
    run.task_ids = vec![
        contract_running.task_id.clone(),
        contract_recoverable.task_id.clone(),
    ];
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Running);
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        vec!["task-running".to_string()]
    );
}

#[test]
fn orchestration_dispatch_run_leases_active_wave() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract_a = reliability_contract("task-a", Vec::new());
    let contract_b = reliability_contract("task-b", Vec::new());
    let contract_c = reliability_contract("task-c", Vec::new());

    reliability
        .get_or_create_task(&contract_a)
        .expect("register task a");
    reliability
        .get_or_create_task(&contract_b)
        .expect("register task b");
    reliability
        .get_or_create_task(&contract_c)
        .expect("register task c");

    let mut run = RunStatus::new("run-dispatch", "Dispatch wave", ExecutionTier::Wave);
    run.max_parallelism = 2;
    run.task_ids = vec![
        contract_a.task_id.clone(),
        contract_b.task_id.clone(),
        contract_c.task_id.clone(),
    ];

    let grants =
        dispatch_run_wave(&mut reliability, &mut run, "worker", 120).expect("dispatch run");

    assert_eq!(grants.len(), 2);
    assert_eq!(
        grants
            .iter()
            .map(|grant| grant.task_id.clone())
            .collect::<Vec<_>>(),
        vec!["task-a".to_string(), "task-b".to_string()]
    );
    assert_eq!(
        grants
            .iter()
            .map(|grant| grant.agent_id.clone())
            .collect::<Vec<_>>(),
        vec!["worker:task-a".to_string(), "worker:task-b".to_string()]
    );
    assert_eq!(run.lifecycle, RunLifecycle::Running);
    assert_eq!(run.task_counts.get("leased"), Some(&2));
    assert_eq!(run.task_counts.get("ready"), Some(&1));
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        vec!["task-a".to_string(), "task-b".to_string()]
    );
}

#[test]
fn orchestration_dispatch_run_returns_empty_when_wave_is_in_flight() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract_a = reliability_contract("task-a", Vec::new());
    let contract_b = reliability_contract("task-b", Vec::new());

    reliability
        .get_or_create_task(&contract_a)
        .expect("register task a");
    reliability
        .get_or_create_task(&contract_b)
        .expect("register task b");

    let mut run = RunStatus::new("run-dispatch", "Dispatch wave", ExecutionTier::Wave);
    run.max_parallelism = 2;
    run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];

    let first_grants =
        dispatch_run_wave(&mut reliability, &mut run, "worker", 120).expect("first dispatch");
    let second_grants = dispatch_run_wave(&mut reliability, &mut run, "worker", 120)
        .expect("repeat dispatch should be idempotent");

    assert_eq!(first_grants.len(), 2);
    assert!(second_grants.is_empty());
    assert_eq!(run.lifecycle, RunLifecycle::Running);
    assert_eq!(run.task_counts.get("leased"), Some(&2));
}

#[test]
fn orchestration_cancel_live_run_tasks_expires_leases_and_clears_active_wave() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract_a = reliability_contract("task-a", Vec::new());
    let contract_b = reliability_contract("task-b", Vec::new());

    reliability
        .get_or_create_task(&contract_a)
        .expect("register task a");
    reliability
        .get_or_create_task(&contract_b)
        .expect("register task b");

    let mut run = RunStatus::new("run-cancel", "Cancel run", ExecutionTier::Wave);
    run.max_parallelism = 2;
    run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];

    let grants =
        dispatch_run_wave(&mut reliability, &mut run, "worker", 120).expect("dispatch run");

    assert_eq!(grants.len(), 2);
    assert_eq!(run.lifecycle, RunLifecycle::Running);
    assert!(run.active_wave.is_some());

    let canceled_grants = cancel_live_run_tasks(&mut reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Canceled);
    assert!(run.active_wave.is_none());
    assert!(run.active_subrun_id.is_none());
    assert_eq!(run.task_counts.get("ready"), Some(&2));
    assert_eq!(canceled_grants.len(), 2);
    assert!(matches!(
        reliability
            .tasks
            .get("task-a")
            .map(|task| &task.runtime.state),
        Some(reliability::RuntimeState::Ready)
    ));
    assert!(matches!(
        reliability
            .tasks
            .get("task-b")
            .map(|task| &task.runtime.state),
        Some(reliability::RuntimeState::Ready)
    ));
}

#[test]
fn orchestration_cancel_live_run_tasks_and_sync_updates_reports_and_session_entries() {
    let (temp_dir, repo_path, _) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        false,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut run = RunStatus::new(
            "run-cancel-sync",
            "Cancel run sync fixture",
            ExecutionTier::Wave,
        );
        run.max_parallelism = 2;
        run.task_ids = vec!["task-a".to_string(), "task-b".to_string()];

        let grants = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.get_or_create_task(&reliability_contract("task-a", Vec::new()))
                .expect("register task a");
            rel.get_or_create_task(&reliability_contract("task-b", Vec::new()))
                .expect("register task b");
            dispatch_run_wave(&mut rel, &mut run, "worker", 120).expect("dispatch run")
        };

        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist run");
        append_dispatch_grants_session_entries(&cx, &session, &reliability_state, &grants)
            .await
            .expect("append dispatch entries");

        cancel_live_run_tasks_and_sync(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &mut run,
        )
        .await
        .expect("cancel live run");

        assert_eq!(run.lifecycle, RunLifecycle::Canceled);
        assert_eq!(run.task_reports.len(), 2);
        assert!(run.active_wave.is_none());
        assert!(
            run.task_reports
                .values()
                .all(|report| report.summary.contains("canceled dispatch"))
        );

        let persisted = run_store.load(&run.run_id).expect("load persisted run");
        assert_eq!(persisted.lifecycle, RunLifecycle::Canceled);
        assert_eq!(persisted.task_reports.len(), 2);

        let snapshot = {
            let guard = session.lock(&cx).await.expect("lock session");
            let inner = guard.session.lock(&cx).await.expect("lock inner session");
            inner.export_snapshot()
        };
        let transitions = snapshot
            .entries
            .into_iter()
            .filter_map(|entry| match entry {
                crate::session::SessionEntry::TaskTransition(entry) => Some(entry),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(transitions.iter().any(|entry| {
            entry.task_id == "task-a"
                && entry.from.as_deref() == Some("leased")
                && entry.to == "ready"
                && entry
                    .details
                    .as_ref()
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    == Some("orchestration.cancel_run")
        }));
        assert!(transitions.iter().any(|entry| {
            entry.task_id == "task-b"
                && entry.from.as_deref() == Some("leased")
                && entry.to == "ready"
                && entry
                    .details
                    .as_ref()
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    == Some("orchestration.cancel_run")
        }));
    });
}

#[test]
fn orchestration_refresh_run_keeps_canceled_wave_cleared() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    reliability
        .get_or_create_task(&reliability_contract("task-a", Vec::new()))
        .expect("register task a");
    reliability
        .get_or_create_task(&reliability_contract("task-b", Vec::new()))
        .expect("register task b");

    let mut run = RunStatus::new(
        "run-canceled-refresh",
        "Canceled refresh fixture",
        ExecutionTier::Wave,
    );
    run.task_ids = vec!["task-a".to_string(), "task-b".to_string()];
    run.lifecycle = RunLifecycle::Canceled;
    run.active_wave = Some(WaveStatus {
        wave_id: "wave-stale".to_string(),
        task_ids: vec!["task-a".to_string()],
        started_at: chrono::Utc::now(),
        completed_at: None,
    });
    run.active_subrun_id = Some("subrun-stale".to_string());

    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Canceled);
    assert!(run.active_wave.is_none());
    assert!(run.active_subrun_id.is_none());
}

#[test]
fn orchestration_refresh_run_serializes_when_task_lacks_planned_touches() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract_a = reliability_contract("task-a", Vec::new());
    let contract_b = reliability_contract("task-b", Vec::new());

    reliability
        .get_or_create_task(&contract_a)
        .expect("register task a");
    reliability
        .get_or_create_task(&contract_b)
        .expect("register task b");
    reliability
        .tasks
        .get_mut("task-a")
        .expect("task-a")
        .spec
        .planned_touches
        .clear();

    let mut run = RunStatus::new("run-serialized", "Serialized wave", ExecutionTier::Wave);
    run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Pending);
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        vec!["task-a".to_string()]
    );
}

#[test]
fn orchestration_refresh_run_respects_max_parallelism_cap() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let mut task_ids = Vec::new();
    for index in 0..5 {
        let task_id = format!("task-cap-{index:02}");
        let contract = reliability_contract(&task_id, Vec::new());
        reliability
            .get_or_create_task(&contract)
            .expect("register capped task");
        task_ids.push(task_id);
    }

    let mut run = RunStatus::new("run-cap", "Capped wave", ExecutionTier::Wave);
    run.max_parallelism = 2;
    run.task_ids = task_ids;
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.lifecycle, RunLifecycle::Pending);
    assert_eq!(run.max_parallelism, 2);
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        vec!["task-cap-00".to_string(), "task-cap-01".to_string()]
    );
}

#[test]
fn orchestration_refresh_run_packs_hierarchical_subruns() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let mut task_ids = Vec::new();
    for index in 0..13 {
        let task_id = format!("task-{index:02}");
        let contract = reliability_contract(&task_id, Vec::new());
        reliability
            .get_or_create_task(&contract)
            .expect("register hierarchical task");
        task_ids.push(task_id);
    }

    let mut run = RunStatus::new("run-hier", "Hierarchical", ExecutionTier::Hierarchical);
    run.max_parallelism = 12;
    run.task_ids = task_ids.clone();
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.active_subrun_id.as_deref(), Some("subrun-01"));
    assert_eq!(run.max_parallelism, 12);
    assert_eq!(run.planned_subruns.len(), 2);
    assert_eq!(run.planned_subruns[0].task_ids.len(), 12);
    assert_eq!(run.planned_subruns[1].task_ids.len(), 1);
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        (0..12)
            .map(|index| format!("task-{index:02}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn orchestration_refresh_run_hierarchical_scope_respects_dependency_order() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let mut task_ids = Vec::new();
    let mut previous: Option<String> = None;
    for index in 0..13 {
        let task_id = format!("task-chain-{index:02}");
        let prerequisites: Vec<_> = previous
            .iter()
            .map(|prior| TaskPrerequisite {
                task_id: prior.clone(),
                trigger: reliability::EdgeTrigger::OnSuccess,
            })
            .collect();
        let contract = reliability_contract(&task_id, prerequisites);
        reliability
            .get_or_create_task(&contract)
            .expect("register chain task");
        reliability
            .reconcile_prerequisites(&contract)
            .expect("reconcile prerequisites");
        previous = Some(task_id.clone());
        task_ids.push(task_id);
    }
    reliability.refresh_dependency_states();

    let mut run = RunStatus::new(
        "run-hier-chain",
        "Hierarchical chain",
        ExecutionTier::Hierarchical,
    );
    run.task_ids = task_ids.clone();
    refresh_run_from_reliability(&reliability, &mut run);

    assert_eq!(run.planned_subruns.len(), 2);
    assert_eq!(run.active_subrun_id.as_deref(), Some("subrun-01"));
    assert_eq!(
        run.active_wave
            .as_ref()
            .map(|wave| wave.task_ids.clone())
            .unwrap_or_default(),
        vec!["task-chain-00".to_string()]
    );
}

#[test]
fn orchestration_submit_rollup_marks_success_and_captures_task_report() {
    let mut reliability = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let contract = reliability_contract("task-success", Vec::new());
    let grant = reliability
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");
    let evidence = reliability
        .append_evidence(AppendEvidenceRequest {
            task_id: contract.task_id.clone(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("evidence");
    let req = SubmitTaskRequest {
        task_id: contract.task_id.clone(),
        lease_id: grant.lease_id,
        fence_token: grant.fence_token,
        patch_digest: "sha256:success".to_string(),
        verify_run_id: "vr-success".to_string(),
        verify_passed: Some(true),
        verify_timed_out: false,
        failure_class: None,
        changed_files: vec!["src/rpc.rs".to_string()],
        symbol_drift_violations: Vec::new(),
        close: Some(ClosePayload {
            task_id: contract.task_id.clone(),
            outcome: "Implemented orchestration report".to_string(),
            outcome_kind: Some(reliability::CloseOutcomeKind::Success),
            acceptance_ids: contract.acceptance_ids.clone(),
            evidence_ids: vec![evidence.evidence_id.clone()],
            trace_parent: contract.parent_goal_trace_id.clone(),
        }),
    };
    let result = reliability.submit_task(req.clone()).expect("submit");
    let report = build_submit_task_report(&reliability, &req, &result).expect("task report");

    let mut orchestration = RpcOrchestrationState::default();
    let mut run = RunStatus::new("run-success", "Run success", ExecutionTier::Inline);
    run.task_ids = vec![contract.task_id.clone()];
    orchestration.register_run(run);

    let updated = refresh_task_runs(
        &reliability,
        &mut orchestration,
        &contract.task_id,
        Some(report),
    );
    let run = updated.first().expect("updated run");
    let task_report = run
        .task_reports
        .get(&contract.task_id)
        .expect("persisted task report");

    assert_eq!(run.lifecycle, RunLifecycle::Succeeded);
    assert_eq!(task_report.summary, "Implemented orchestration report");
    assert_eq!(task_report.verify_exit_code, 0);
    assert_eq!(task_report.changed_files, vec!["src/rpc.rs".to_string()]);
}

#[test]
fn orchestration_inline_execution_substrate_applies_verified_diff_and_closes_task() {
    let (temp_dir, repo_path, head) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract = reliability_contract("task-inline-exec", Vec::new());
        contract.input_snapshot = Some(head);
        contract.verify_command = "grep -q updated src/lib.rs".to_string();
        contract.planned_touches = vec!["src/lib.rs".to_string()];

        let grant = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.request_dispatch(&contract, "worker", 120)
                .expect("dispatch inline task")
        };

        let mut run = RunStatus::new(
            "run-inline-exec",
            "Inline execution fixture",
            ExecutionTier::Inline,
        );
        run.task_ids = vec![contract.task_id.clone()];
        run.run_verify_command = "true".to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let worker = FixtureInlineWorker {
            target: "src/lib.rs".to_string(),
            contents: "pub fn fixture() { println!(\"updated\"); }\n".to_string(),
            summary: "Updated the fixture implementation".to_string(),
        };
        let updated_run = execute_inline_dispatch_grant_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            "run-inline-exec",
            &grant,
            &worker,
        )
        .await
        .expect("execute inline task");

        assert_eq!(updated_run.lifecycle, RunLifecycle::Succeeded);
        assert!(
            updated_run
                .latest_run_verify
                .as_ref()
                .is_some_and(|verify| verify.ok)
        );
        assert_eq!(
            updated_run
                .task_reports
                .get(&contract.task_id)
                .expect("task report")
                .verify_exit_code,
            0
        );

        let parent_contents =
            fs::read_to_string(repo_path.join("src/lib.rs")).expect("read parent file");
        assert!(parent_contents.contains("updated"));

        let rel = reliability_state.lock(&cx).await.expect("lock reliability");
        assert!(matches!(
            rel.tasks
                .get(&contract.task_id)
                .map(|task| &task.runtime.state),
            Some(reliability::RuntimeState::Terminal(
                reliability::TerminalState::Succeeded { .. }
            ))
        ));
    });
}

#[test]
fn orchestration_inline_execution_substrate_keeps_parent_repo_clean_on_verify_failure() {
    let (temp_dir, repo_path, head) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract = reliability_contract("task-inline-fail", Vec::new());
        contract.input_snapshot = Some(head);
        contract.verify_command = "grep -q missing src/lib.rs".to_string();
        contract.planned_touches = vec!["src/lib.rs".to_string()];

        let grant = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.request_dispatch(&contract, "worker", 120)
                .expect("dispatch inline task")
        };

        let mut run = RunStatus::new(
            "run-inline-fail",
            "Inline execution verify failure",
            ExecutionTier::Inline,
        );
        run.task_ids = vec![contract.task_id.clone()];
        run.run_verify_command = "true".to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let worker = FixtureInlineWorker {
            target: "src/lib.rs".to_string(),
            contents: "pub fn fixture() { println!(\"updated\"); }\n".to_string(),
            summary: "Attempted inline update".to_string(),
        };
        let err = execute_inline_dispatch_grant_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            "run-inline-fail",
            &grant,
            &worker,
        )
        .await
        .expect_err("hard-mode verify failure should reject close");
        assert!(err.to_string().contains("close rejected"));

        let parent_contents =
            fs::read_to_string(repo_path.join("src/lib.rs")).expect("read parent file");
        assert!(parent_contents.contains("before"));
        assert!(!parent_contents.contains("updated"));

        let rel = reliability_state.lock(&cx).await.expect("lock reliability");
        assert!(matches!(
            rel.tasks
                .get(&contract.task_id)
                .map(|task| &task.runtime.state),
            Some(reliability::RuntimeState::Recoverable {
                reason: reliability::FailureClass::InfraTransient,
                retry_after: Some(_),
                ..
            })
        ));
    });
}

#[test]
fn orchestration_inline_execution_failure_persists_rollback_report() {
    let (temp_dir, repo_path, head) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract = reliability_contract("task-inline-worker-fail", Vec::new());
        contract.input_snapshot = Some(head);
        contract.verify_command = "true".to_string();
        contract.planned_touches = vec!["src/lib.rs".to_string()];

        let grant = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.request_dispatch(&contract, "worker", 120)
                .expect("dispatch inline task")
        };

        let mut run = RunStatus::new(
            "run-inline-worker-fail",
            "Inline execution worker failure",
            ExecutionTier::Inline,
        );
        run.task_ids = vec![contract.task_id.clone()];
        run.run_verify_command = "true".to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let worker = FailingInlineWorker {
            message: "fixture worker exploded".to_string(),
        };
        let err = execute_inline_dispatch_grant_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            "run-inline-worker-fail",
            &grant,
            &worker,
        )
        .await
        .expect_err("worker failure should surface");
        assert!(err.to_string().contains("fixture worker exploded"));

        let run = current_run_status(
            &cx,
            &orchestration_state,
            &run_store,
            "run-inline-worker-fail",
        )
        .await
        .expect("load updated run");
        let report = run
            .task_reports
            .get(&contract.task_id)
            .expect("rollback task report");

        assert_eq!(run.lifecycle, RunLifecycle::Pending);
        assert_eq!(
            report.failure_class.as_deref(),
            Some("orchestration_execution_error")
        );
        assert_eq!(report.verify_exit_code, -1);
        assert!(report.summary.contains("fixture worker exploded"));

        let rel = reliability_state.lock(&cx).await.expect("lock reliability");
        assert!(matches!(
            rel.tasks
                .get(&contract.task_id)
                .map(|task| &task.runtime.state),
            Some(reliability::RuntimeState::Recoverable {
                reason: reliability::FailureClass::InfraTransient,
                retry_after: Some(_),
                ..
            })
        ));

        let snapshot = {
            let guard = session.lock(&cx).await.expect("lock session");
            let inner = guard.session.lock(&cx).await.expect("lock inner session");
            inner.export_snapshot()
        };
        let transitions = snapshot
            .entries
            .into_iter()
            .filter_map(|entry| match entry {
                crate::session::SessionEntry::TaskTransition(entry) => Some(entry),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(transitions.iter().any(|entry| {
            entry.task_id == contract.task_id
                && entry.from.as_deref() == Some("leased")
                && entry.to == "recoverable"
                && entry
                    .details
                    .as_ref()
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    == Some("orchestration.rollback_dispatch_grant")
        }));
    });
}

#[test]
fn orchestration_inline_execution_substrate_applies_created_file_and_closes_task() {
    let (temp_dir, repo_path, head) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract = reliability_contract("task-inline-create", Vec::new());
        contract.input_snapshot = Some(head);
        contract.verify_command = "test -f src/new_file.rs".to_string();
        contract.planned_touches = vec!["src/new_file.rs".to_string()];

        let grant = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.request_dispatch(&contract, "worker", 120)
                .expect("dispatch inline create task")
        };

        let mut run = RunStatus::new(
            "run-inline-create",
            "Inline execution create-file fixture",
            ExecutionTier::Inline,
        );
        run.task_ids = vec![contract.task_id.clone()];
        run.run_verify_command = "test -f src/new_file.rs".to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let worker = FixtureInlineWorker {
            target: "src/new_file.rs".to_string(),
            contents: "pub fn created_fixture() {}\n".to_string(),
            summary: "Created a new fixture file".to_string(),
        };
        let updated_run = execute_inline_dispatch_grant_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            "run-inline-create",
            &grant,
            &worker,
        )
        .await
        .expect("execute inline create task");

        assert_eq!(updated_run.lifecycle, RunLifecycle::Succeeded);
        assert!(
            updated_run
                .latest_run_verify
                .as_ref()
                .is_some_and(|verify| verify.ok)
        );
        assert_eq!(
            updated_run
                .task_reports
                .get(&contract.task_id)
                .expect("task report")
                .verify_exit_code,
            0
        );

        let created_contents =
            fs::read_to_string(repo_path.join("src/new_file.rs")).expect("read created file");
        assert!(created_contents.contains("created_fixture"));

        let rel = reliability_state.lock(&cx).await.expect("lock reliability");
        assert!(matches!(
            rel.tasks
                .get(&contract.task_id)
                .map(|task| &task.runtime.state),
            Some(reliability::RuntimeState::Terminal(
                reliability::TerminalState::Succeeded { .. }
            ))
        ));
    });
}

#[test]
fn orchestration_parallel_wave_capture_executes_workers_concurrently() {
    let (temp_dir, repo_path, _) = setup_inline_execution_repo();
    fs::write(
        repo_path.join("src/extra.rs"),
        "pub fn extra_fixture() { println!(\"before-extra\"); }\n",
    )
    .expect("write extra tracked file");
    run_git(&repo_path, &["add", "."]);
    run_git(&repo_path, &["commit", "-m", "add extra tracked file"]);
    let head = git_stdout(&repo_path, &["rev-parse", "HEAD"]);
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract_a = reliability_contract("task-parallel-a", Vec::new());
        contract_a.input_snapshot = Some(head.clone());
        contract_a.verify_command = "true".to_string();
        contract_a.planned_touches = vec!["src/lib.rs".to_string()];

        let mut contract_b = reliability_contract("task-parallel-b", Vec::new());
        contract_b.input_snapshot = Some(head);
        contract_b.verify_command = "true".to_string();
        contract_b.planned_touches = vec!["src/extra.rs".to_string()];

        let grants = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            vec![
                rel.request_dispatch(&contract_a, "worker-a", 120)
                    .expect("dispatch parallel task a"),
                rel.request_dispatch(&contract_b, "worker-b", 120)
                    .expect("dispatch parallel task b"),
            ]
        };

        let mut run = RunStatus::new(
            "run-parallel-wave",
            "Parallel wave execution fixture",
            ExecutionTier::Wave,
        );
        run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
        run.run_verify_command =
            "grep -q task_parallel_a src/lib.rs && grep -q task_parallel_b src/extra.rs"
                .to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let worker = ConcurrentFixtureInlineWorker {
            in_flight: Arc::clone(&in_flight),
            max_in_flight: Arc::clone(&max_in_flight),
            sleep: Duration::from_millis(50),
        };

        let updated_run = execute_dispatch_grants_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            run,
            &grants,
            &worker,
        )
        .await
        .expect("execute parallel wave");

        assert_eq!(updated_run.lifecycle, RunLifecycle::Succeeded);
        assert!(
            updated_run
                .latest_run_verify
                .as_ref()
                .is_some_and(|verify| verify.ok)
        );
        assert_eq!(max_in_flight.load(Ordering::SeqCst), 2);
        let lib_contents =
            fs::read_to_string(repo_path.join("src/lib.rs")).expect("read updated lib file");
        let extra_contents =
            fs::read_to_string(repo_path.join("src/extra.rs")).expect("read updated extra file");
        assert!(lib_contents.contains("task_parallel_a"));
        assert!(extra_contents.contains("task_parallel_b"));
    });
}

#[test]
fn orchestration_parallel_wave_salvages_successful_captures_after_partial_failure() {
    let (temp_dir, repo_path, _) = setup_inline_execution_repo();
    fs::write(
        repo_path.join("src/extra.rs"),
        "pub fn extra_fixture() { println!(\"before-extra\"); }\n",
    )
    .expect("write extra tracked file");
    run_git(&repo_path, &["add", "."]);
    run_git(&repo_path, &["commit", "-m", "add extra tracked file"]);
    let head = git_stdout(&repo_path, &["rev-parse", "HEAD"]);
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract_a = reliability_contract("task-partial-a", Vec::new());
        contract_a.input_snapshot = Some(head.clone());
        contract_a.verify_command = "true".to_string();
        contract_a.planned_touches = vec!["src/lib.rs".to_string()];

        let mut contract_b = reliability_contract("task-partial-b", Vec::new());
        contract_b.input_snapshot = Some(head);
        contract_b.verify_command = "true".to_string();
        contract_b.planned_touches = vec!["src/extra.rs".to_string()];

        let grants = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            vec![
                rel.request_dispatch(&contract_a, "worker-a", 120)
                    .expect("dispatch partial task a"),
                rel.request_dispatch(&contract_b, "worker-b", 120)
                    .expect("dispatch partial task b"),
            ]
        };

        let mut run = RunStatus::new(
            "run-partial-wave",
            "Parallel partial failure fixture",
            ExecutionTier::Wave,
        );
        run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
        run.run_verify_command = "true".to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let worker = PartialWaveFailureWorker {
            failing_task_id: "task-partial-b".to_string(),
        };
        let updated_run = execute_dispatch_grants_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            run,
            &grants,
            &worker,
        )
        .await
        .expect("partial wave failure should stay automatable");
        assert_eq!(updated_run.lifecycle, RunLifecycle::Pending);

        let lib_contents =
            fs::read_to_string(repo_path.join("src/lib.rs")).expect("read updated lib file");
        let extra_contents =
            fs::read_to_string(repo_path.join("src/extra.rs")).expect("read extra file");
        assert!(lib_contents.contains("task_partial_a"));
        assert!(!extra_contents.contains("task_partial_b"));

        let updated_run =
            current_run_status(&cx, &orchestration_state, &run_store, "run-partial-wave")
                .await
                .expect("load updated run");
        assert_eq!(updated_run.lifecycle, RunLifecycle::Pending);
        assert_eq!(
            updated_run
                .task_reports
                .get("task-partial-a")
                .and_then(|report| report.failure_class.clone()),
            None
        );
        assert_eq!(
            updated_run
                .task_reports
                .get("task-partial-b")
                .and_then(|report| report.failure_class.as_deref()),
            Some("orchestration_execution_error")
        );
        assert!(
            updated_run
                .task_reports
                .get("task-partial-b")
                .is_some_and(|report| report.summary.contains("fixture partial wave failure"))
        );

        let rel = reliability_state.lock(&cx).await.expect("lock reliability");
        assert!(matches!(
            rel.tasks
                .get("task-partial-a")
                .map(|task| &task.runtime.state),
            Some(reliability::RuntimeState::Terminal(
                reliability::TerminalState::Succeeded { .. }
            ))
        ));
        assert!(matches!(
            rel.tasks
                .get("task-partial-b")
                .map(|task| &task.runtime.state),
            Some(reliability::RuntimeState::Recoverable {
                reason: reliability::FailureClass::InfraTransient,
                retry_after: Some(_),
                ..
            })
        ));
    });
}

#[test]
fn orchestration_overlap_replay_uses_merged_base_state() {
    let (temp_dir, repo_path, head) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract_a = reliability_contract("task-overlap-a", Vec::new());
        contract_a.input_snapshot = Some(head.clone());
        contract_a.verify_command = "true".to_string();
        contract_a.planned_touches = vec!["src/lib.rs".to_string()];

        let mut contract_b = reliability_contract("task-overlap-b", Vec::new());
        contract_b.input_snapshot = Some(head);
        contract_b.verify_command = "true".to_string();
        contract_b.planned_touches = vec!["src/lib.rs".to_string()];

        let grants = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            vec![
                rel.request_dispatch(&contract_a, "worker-a", 120)
                    .expect("dispatch overlap task a"),
                rel.request_dispatch(&contract_b, "worker-b", 120)
                    .expect("dispatch overlap task b"),
            ]
        };

        let mut run = RunStatus::new(
            "run-overlap-replay",
            "Overlap replay fixture",
            ExecutionTier::Wave,
        );
        run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
        run.run_verify_command =
            "grep -q \"beta-after-alpha\" src/lib.rs && ! grep -q \"beta-alone\" src/lib.rs"
                .to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let worker = OverlapReplayFixtureWorker {
            target: "src/lib.rs".to_string(),
        };
        let updated_run = execute_dispatch_grants_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            run,
            &grants,
            &worker,
        )
        .await
        .expect("execute overlap replay");

        assert_eq!(updated_run.lifecycle, RunLifecycle::Succeeded);
        assert!(
            updated_run
                .latest_run_verify
                .as_ref()
                .is_some_and(|verify| verify.ok)
        );

        let lib_contents =
            fs::read_to_string(repo_path.join("src/lib.rs")).expect("read updated lib file");
        assert!(lib_contents.contains("// alpha"));
        assert!(lib_contents.contains("// beta-after-alpha"));
        assert!(!lib_contents.contains("// beta-alone"));
    });
}

#[test]
fn orchestration_sequential_dispatch_rebases_on_parent_worktree_base() {
    let (temp_dir, repo_path, head) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let mut contract_a = reliability_contract("task-overlap-a", Vec::new());
        contract_a.input_snapshot = Some(head.clone());
        contract_a.verify_command = "true".to_string();
        contract_a.planned_touches = vec!["src/lib.rs".to_string()];

        let mut contract_b = reliability_contract("task-overlap-b", Vec::new());
        contract_b.input_snapshot = Some(head);
        contract_b.verify_command = "true".to_string();
        contract_b.planned_touches = vec!["src/lib.rs".to_string()];

        {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.get_or_create_task(&contract_b)
                .expect("register second sequential task");
        }

        let grant_a = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.request_dispatch(&contract_a, "worker-a", 120)
                .expect("dispatch first sequential task")
        };

        let mut run = RunStatus::new(
            "run-sequential-rebase",
            "Sequential rebase fixture",
            ExecutionTier::Wave,
        );
        run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
        run.run_verify_command = "true".to_string();
        run.run_verify_timeout_sec = Some(30);
        {
            let rel = reliability_state.lock(&cx).await.expect("lock reliability");
            refresh_run_from_reliability(&rel, &mut run);
        }
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let worker = OverlapReplayFixtureWorker {
            target: "src/lib.rs".to_string(),
        };
        let after_first = execute_inline_dispatch_grant_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            "run-sequential-rebase",
            &grant_a,
            &worker,
        )
        .await
        .expect("execute first sequential task");
        assert!(matches!(
            after_first.lifecycle,
            RunLifecycle::Pending | RunLifecycle::Running
        ));

        let after_first_contents =
            fs::read_to_string(repo_path.join("src/lib.rs")).expect("read first-wave file");
        assert!(after_first_contents.contains("// alpha"));
        assert!(!after_first_contents.contains("// beta-after-alpha"));

        let grant_b = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.request_dispatch(&contract_b, "worker-b", 120)
                .expect("dispatch second sequential task")
        };

        let updated_run = execute_inline_dispatch_grant_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            "run-sequential-rebase",
            &grant_b,
            &worker,
        )
        .await
        .expect("execute second sequential task");

        assert_eq!(updated_run.lifecycle, RunLifecycle::Succeeded);
        let lib_contents =
            fs::read_to_string(repo_path.join("src/lib.rs")).expect("read updated lib file");
        assert!(lib_contents.contains("// alpha"));
        assert!(lib_contents.contains("// beta-after-alpha"));
        assert!(!lib_contents.contains("// beta-alone"));
    });
}

#[test]
fn orchestration_dispatch_wave_advances_same_touch_prerequisite_chain() {
    let (temp_dir, repo_path, head) = setup_inline_execution_repo();
    let run_store = RunStore::new(temp_dir.path().join("runs"));
    let cx = AgentCx::for_request();
    let session = build_test_agent_session(&repo_path);
    let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
        ReliabilityEnforcementMode::Hard,
        true,
        true,
    )));
    let orchestration_state = Arc::new(Mutex::new(RpcOrchestrationState::default()));

    run_async(async {
        let contract_a = TaskContract {
            input_snapshot: Some(head.clone()),
            verify_command: "true".to_string(),
            planned_touches: vec!["src/lib.rs".to_string()],
            ..reliability_contract("task-overlap-a", Vec::new())
        };
        let contract_b = TaskContract {
            input_snapshot: Some(head),
            verify_command: "true".to_string(),
            planned_touches: vec!["src/lib.rs".to_string()],
            prerequisites: vec![TaskPrerequisite {
                task_id: "task-overlap-a".to_string(),
                trigger: reliability::EdgeTrigger::OnSuccess,
            }],
            ..reliability_contract("task-overlap-b", Vec::new())
        };

        let mut run = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            rel.get_or_create_task(&contract_a)
                .expect("register first chain task");
            rel.get_or_create_task(&contract_b)
                .expect("register second chain task");
            rel.reconcile_prerequisites(&contract_b)
                .expect("reconcile prerequisite");

            let mut run = RunStatus::new(
                "run-chain-wave",
                "Same-touch prerequisite chain",
                ExecutionTier::Wave,
            );
            run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
            refresh_run_from_reliability(&rel, &mut run);
            run
        };
        {
            let mut orchestration = orchestration_state
                .lock(&cx)
                .await
                .expect("lock orchestration");
            orchestration.register_run(run.clone());
        }
        persist_run_status(&cx, &session, &run_store, &run)
            .await
            .expect("persist initial run");

        let first_grant = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            dispatch_run_wave(&mut rel, &mut run, "worker", 120)
                .expect("dispatch first wave")
                .into_iter()
                .next()
                .expect("first wave grant")
        };

        let worker = OverlapReplayFixtureWorker {
            target: "src/lib.rs".to_string(),
        };
        execute_inline_dispatch_grant_with_worker(
            &cx,
            &session,
            &reliability_state,
            &orchestration_state,
            &run_store,
            &repo_path,
            "run-chain-wave",
            &first_grant,
            &worker,
        )
        .await
        .expect("execute first wave");

        let mut run = current_run_status(&cx, &orchestration_state, &run_store, "run-chain-wave")
            .await
            .expect("load updated run");
        let second_grants = {
            let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
            dispatch_run_wave(&mut rel, &mut run, "worker", 120).expect("dispatch second wave")
        };

        assert_eq!(second_grants.len(), 1);
        assert_eq!(second_grants[0].task_id, "task-overlap-b");
    });
}

#[test]
fn orchestration_dispatch_workspace_segment_id_distinguishes_parallel_grants() {
    let left = DispatchGrant {
        task_id: "task-a".to_string(),
        agent_id: "worker-a".to_string(),
        lease_id: "lease-a".to_string(),
        fence_token: 1,
        expires_at: chrono::Utc::now(),
        state: "leased".to_string(),
    };
    let right = DispatchGrant {
        task_id: "task-b".to_string(),
        agent_id: "worker-b".to_string(),
        lease_id: "lease-b".to_string(),
        fence_token: 1,
        expires_at: chrono::Utc::now(),
        state: "leased".to_string(),
    };

    assert_ne!(
        dispatch_workspace_segment_id(&left),
        dispatch_workspace_segment_id(&right)
    );
}

#[test]
fn orchestration_blocker_rollup_marks_awaiting_human() {
    let mut reliability =
        reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract = reliability_contract("task-blocker", Vec::new());
    let grant = reliability
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");
    let state = reliability
        .resolve_blocker(BlockerReport {
            task_id: contract.task_id.clone(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            reason: "Need approval".to_string(),
            context: "Waiting on API credentials".to_string(),
            defer_trigger: None,
            resolved: false,
        })
        .expect("blocker");
    let report = build_runtime_task_report(
        &reliability,
        &contract.task_id,
        format!("Human blocker raised: state={state}"),
    )
    .expect("runtime report");

    let mut orchestration = RpcOrchestrationState::default();
    let mut run = RunStatus::new("run-blocked", "Run blocked", ExecutionTier::Inline);
    run.task_ids = vec![contract.task_id.clone()];
    orchestration.register_run(run);

    let updated = refresh_task_runs(
        &reliability,
        &mut orchestration,
        &contract.task_id,
        Some(report),
    );
    let run = updated.first().expect("updated run");
    let task_report = run
        .task_reports
        .get(&contract.task_id)
        .expect("persisted task report");

    assert_eq!(run.lifecycle, RunLifecycle::AwaitingHuman);
    assert_eq!(task_report.failure_class.as_deref(), Some("human_blocker"));
    assert!(
        task_report
            .blockers
            .iter()
            .any(|blocker: &String| blocker.contains("Need approval"))
    );
}

#[test]
fn orchestration_completed_run_verify_scope_detects_run_completion() {
    let mut reliability = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let contract_a = reliability_contract("task-wave-a", Vec::new());
    let contract_b = reliability_contract("task-wave-b", Vec::new());

    let grant_a = reliability
        .request_dispatch(&contract_a, "agent-a", 60)
        .expect("dispatch task a");
    let grant_b = reliability
        .request_dispatch(&contract_b, "agent-b", 60)
        .expect("dispatch task b");

    for (contract, grant, verify_run_id) in [
        (&contract_a, grant_a, "vr-wave-a"),
        (&contract_b, grant_b, "vr-wave-b"),
    ] {
        let evidence = reliability
            .append_evidence(AppendEvidenceRequest {
                task_id: contract.task_id.clone(),
                command: "cargo test".to_string(),
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
                artifact_ids: Vec::new(),
                env_id: None,
            })
            .expect("append evidence");
        reliability
            .submit_task(SubmitTaskRequest {
                task_id: contract.task_id.clone(),
                lease_id: grant.lease_id,
                fence_token: grant.fence_token,
                patch_digest: format!("sha256:{}", contract.task_id),
                verify_run_id: verify_run_id.to_string(),
                verify_passed: Some(true),
                verify_timed_out: false,
                failure_class: None,
                changed_files: vec![format!("src/{}.rs", contract.task_id)],
                symbol_drift_violations: Vec::new(),
                close: Some(ClosePayload {
                    task_id: contract.task_id.clone(),
                    outcome: format!("Completed {}", contract.task_id),
                    outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                    acceptance_ids: contract.acceptance_ids.clone(),
                    evidence_ids: vec![evidence.evidence_id],
                    trace_parent: contract.parent_goal_trace_id.clone(),
                }),
            })
            .expect("submit success");
    }

    let mut run = RunStatus::new("run-wave-verify", "Run verify", ExecutionTier::Wave);
    run.run_verify_command = "true".to_string();
    run.run_verify_timeout_sec = Some(30);
    run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
    run.active_wave = Some(WaveStatus {
        wave_id: "wave-1".to_string(),
        task_ids: run.task_ids.clone(),
        started_at: chrono::Utc::now(),
        completed_at: None,
    });

    let scope = completed_run_verify_scope(&reliability, &run).expect("completed run scope");
    assert_eq!(scope.scope_id, run.run_id);
    assert_eq!(scope.scope_kind, RunVerifyScopeKind::Run);
    assert_eq!(scope.subrun_id, None);
}

#[test]
fn orchestration_completed_run_verify_scope_waits_for_full_run_completion() {
    let mut reliability = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let contract_a = reliability_contract("task-wave-a", Vec::new());
    let contract_b = reliability_contract(
        "task-wave-b",
        vec![TaskPrerequisite {
            task_id: contract_a.task_id.clone(),
            trigger: reliability::EdgeTrigger::OnSuccess,
        }],
    );

    reliability
        .get_or_create_task(&contract_a)
        .expect("register task a");
    reliability
        .get_or_create_task(&contract_b)
        .expect("register task b");
    reliability
        .reconcile_prerequisites(&contract_b)
        .expect("reconcile prerequisite");

    let grant_a = reliability
        .request_dispatch_existing(&contract_a.task_id, "agent-a", 60)
        .expect("dispatch task a");
    let evidence = reliability
        .append_evidence(AppendEvidenceRequest {
            task_id: contract_a.task_id.clone(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("append evidence");
    reliability
        .submit_task(SubmitTaskRequest {
            task_id: contract_a.task_id.clone(),
            lease_id: grant_a.lease_id,
            fence_token: grant_a.fence_token,
            patch_digest: "sha256:task-wave-a".to_string(),
            verify_run_id: "vr-wave-a".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: vec!["src/task-wave-a.rs".to_string()],
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: contract_a.task_id.clone(),
                outcome: format!("Completed {}", contract_a.task_id),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: contract_a.acceptance_ids.clone(),
                evidence_ids: vec![evidence.evidence_id],
                trace_parent: contract_a.parent_goal_trace_id.clone(),
            }),
        })
        .expect("submit success");
    reliability.refresh_dependency_states();

    let mut run = RunStatus::new(
        "run-wave-intermediate",
        "Run verify waits for terminal run",
        ExecutionTier::Wave,
    );
    run.run_verify_command = "true".to_string();
    run.run_verify_timeout_sec = Some(30);
    run.task_ids = vec![contract_a.task_id.clone(), contract_b.task_id.clone()];
    run.active_wave = Some(WaveStatus {
        wave_id: "wave-1".to_string(),
        task_ids: vec![contract_a.task_id.clone()],
        started_at: chrono::Utc::now(),
        completed_at: None,
    });

    assert!(
        completed_run_verify_scope(&reliability, &run).is_none(),
        "run verify should wait until all run tasks succeed"
    );
}

#[test]
fn orchestration_completed_run_verify_scope_waits_for_full_hierarchical_completion() {
    let mut reliability = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let mut task_ids = Vec::new();
    let mut previous: Option<String> = None;
    for index in 0..13 {
        let task_id = format!("task-hier-chain-{index:02}");
        let prerequisites: Vec<_> = previous
            .iter()
            .map(|prior| TaskPrerequisite {
                task_id: prior.clone(),
                trigger: reliability::EdgeTrigger::OnSuccess,
            })
            .collect();
        let contract = reliability_contract(&task_id, prerequisites);
        reliability
            .get_or_create_task(&contract)
            .expect("register hierarchical chain task");
        reliability
            .reconcile_prerequisites(&contract)
            .expect("reconcile hierarchical prerequisites");
        previous = Some(task_id.clone());
        task_ids.push(task_id);
    }

    for task_id in task_ids.iter().take(12) {
        let contract = reliability_contract(task_id, Vec::new());
        let grant = reliability
            .request_dispatch_existing(task_id, "agent-a", 60)
            .expect("dispatch task");
        let evidence = reliability
            .append_evidence(AppendEvidenceRequest {
                task_id: task_id.clone(),
                command: "cargo test".to_string(),
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
                artifact_ids: Vec::new(),
                env_id: None,
            })
            .expect("append evidence");
        reliability
            .submit_task(SubmitTaskRequest {
                task_id: task_id.clone(),
                lease_id: grant.lease_id,
                fence_token: grant.fence_token,
                patch_digest: format!("sha256:{task_id}"),
                verify_run_id: format!("vr-{task_id}"),
                verify_passed: Some(true),
                verify_timed_out: false,
                failure_class: None,
                changed_files: vec![format!("src/{task_id}.rs")],
                symbol_drift_violations: Vec::new(),
                close: Some(ClosePayload {
                    task_id: task_id.clone(),
                    outcome: format!("Completed {task_id}"),
                    outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                    acceptance_ids: contract.acceptance_ids.clone(),
                    evidence_ids: vec![evidence.evidence_id],
                    trace_parent: contract.parent_goal_trace_id.clone(),
                }),
            })
            .expect("submit success");
    }
    reliability.refresh_dependency_states();

    let mut run = RunStatus::new(
        "run-hier-intermediate",
        "Run verify waits for terminal hierarchical run",
        ExecutionTier::Hierarchical,
    );
    run.run_verify_command = "true".to_string();
    run.run_verify_timeout_sec = Some(30);
    run.task_ids = task_ids.clone();
    refresh_run_from_reliability(&reliability, &mut run);
    assert_eq!(run.planned_subruns.len(), 2);

    let first_subrun = run.planned_subruns.first().cloned().expect("first subrun");
    assert_eq!(first_subrun.task_ids.len(), 12);
    run.active_subrun_id = Some(first_subrun.subrun_id.clone());
    run.active_wave = Some(WaveStatus {
        wave_id: "wave-hier-1".to_string(),
        task_ids: first_subrun.task_ids.clone(),
        started_at: chrono::Utc::now(),
        completed_at: None,
    });

    assert!(
        completed_run_verify_scope(&reliability, &run).is_none(),
        "hierarchical run verify should wait until all run tasks succeed"
    );
}

#[test]
fn orchestration_refresh_run_preserves_failed_run_verification_state() {
    let mut reliability = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let contract = reliability_contract("task-run-verify-fail", Vec::new());
    let grant = reliability
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");
    let evidence = reliability
        .append_evidence(AppendEvidenceRequest {
            task_id: contract.task_id.clone(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("append evidence");
    reliability
        .submit_task(SubmitTaskRequest {
            task_id: contract.task_id.clone(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:run-verify-fail".to_string(),
            verify_run_id: "vr-run-verify-fail".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: vec!["src/rpc.rs".to_string()],
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: contract.task_id.clone(),
                outcome: "Completed run verify failure fixture".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: contract.acceptance_ids.clone(),
                evidence_ids: vec![evidence.evidence_id],
                trace_parent: contract.parent_goal_trace_id.clone(),
            }),
        })
        .expect("submit success");

    let mut run = RunStatus::new(
        "run-verify-fail-state",
        "Run verify failed",
        ExecutionTier::Inline,
    );
    run.task_ids = vec![contract.task_id.clone()];
    run.latest_run_verify = Some(RunVerifyStatus {
        scope_id: "run-verify-fail-state".to_string(),
        scope_kind: RunVerifyScopeKind::Run,
        subrun_id: None,
        command: "false".to_string(),
        timeout_sec: 30,
        exit_code: 1,
        ok: false,
        summary: "run verification failed".to_string(),
        duration_ms: 5,
        generated_at: chrono::Utc::now(),
    });

    refresh_run_from_reliability(&reliability, &mut run);
    assert_eq!(run.lifecycle, RunLifecycle::Failed);
}

#[test]
fn orchestration_execute_run_verification_records_success() {
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .blocking_threads(1, 8)
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut run = RunStatus::new(
            "run-verify-success",
            "Run verify success",
            ExecutionTier::Wave,
        );
        run.run_verify_command = "true".to_string();
        run.run_verify_timeout_sec = Some(30);
        let scope = CompletedRunVerifyScope {
            scope_id: "run-verify-success".to_string(),
            scope_kind: RunVerifyScopeKind::Run,
            subrun_id: None,
        };

        execute_run_verification(temp_dir.path(), &mut run, &scope).await;

        let verify = run.latest_run_verify.as_ref().expect("verification result");
        assert!(verify.ok);
        assert_eq!(verify.scope_id, "run-verify-success");
        assert_eq!(run.lifecycle, RunLifecycle::Pending);
    });
}

#[test]
fn orchestration_execute_run_verification_records_failure() {
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .blocking_threads(1, 8)
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut run = RunStatus::new(
            "run-verify-failure",
            "Run verify failure",
            ExecutionTier::Wave,
        );
        run.run_verify_command = "false".to_string();
        run.run_verify_timeout_sec = Some(30);
        let scope = CompletedRunVerifyScope {
            scope_id: "run-verify-failure".to_string(),
            scope_kind: RunVerifyScopeKind::Run,
            subrun_id: None,
        };

        execute_run_verification(temp_dir.path(), &mut run, &scope).await;

        let verify = run.latest_run_verify.as_ref().expect("verification result");
        assert!(!verify.ok);
        assert_eq!(verify.scope_id, "run-verify-failure");
        assert_eq!(run.lifecycle, RunLifecycle::Failed);
    });
}

#[test]
fn line_count_from_newline_count_matches_trailing_newline_semantics() {
    assert_eq!(line_count_from_newline_count(0, 0, false), 0);
    assert_eq!(line_count_from_newline_count(2, 1, true), 1);
    assert_eq!(line_count_from_newline_count(1, 0, false), 1);
    assert_eq!(line_count_from_newline_count(3, 1, false), 2);
}

#[test]
fn reliability_submit_task_sets_retry_after_when_open_ended_defer_disabled() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, false);
    let contract = TaskContract {
        task_id: "task-1".to_string(),
        objective: "Objective".to_string(),
        parent_goal_trace_id: Some("goal:task-1".to_string()),
        invariants: Vec::new(),
        max_touched_files: None,
        forbid_paths: Vec::new(),
        verify_command: "cargo test".to_string(),
        verify_timeout_sec: Some(30),
        max_attempts: Some(3),
        input_snapshot: Some("snapshot".to_string()),
        acceptance_ids: vec!["ac-1".to_string()],
        planned_touches: vec!["src/task_1.rs".to_string()],
        prerequisites: Vec::new(),
        enforce_symbol_drift_check: false,
    };
    let grant = state
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");

    state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-1".to_string(),
            command: "cargo test".to_string(),
            exit_code: 1,
            stdout: String::new(),
            stderr: "boom".to_string(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("evidence");

    let _ = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-1".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:abc".to_string(),
            verify_run_id: "vr-1".to_string(),
            verify_passed: Some(false),
            verify_timed_out: false,
            failure_class: Some(reliability::FailureClass::VerificationFailed),
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: None,
        })
        .expect("submit should transition to recoverable");

    let task = state.tasks.get("task-1").expect("task");
    assert!(
        matches!(
            task.runtime.state,
            reliability::RuntimeState::Recoverable {
                retry_after: Some(_),
                ..
            }
        ),
        "recoverable task should get retry_after when open-ended defer is disabled"
    );
}

#[test]
fn reliability_submit_task_rejects_unsafe_success_close_outcome_in_hard_mode() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let contract = TaskContract {
        task_id: "task-2".to_string(),
        objective: "Objective".to_string(),
        parent_goal_trace_id: Some("goal:task-2".to_string()),
        invariants: Vec::new(),
        max_touched_files: None,
        forbid_paths: Vec::new(),
        verify_command: "cargo test".to_string(),
        verify_timeout_sec: Some(30),
        max_attempts: Some(3),
        input_snapshot: Some("snapshot".to_string()),
        acceptance_ids: vec!["ac-1".to_string()],
        planned_touches: vec!["src/task_2.rs".to_string()],
        prerequisites: Vec::new(),
        enforce_symbol_drift_check: false,
    };
    let grant = state
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");

    let evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-2".to_string(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("evidence");

    let err = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-2".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:def".to_string(),
            verify_run_id: "vr-2".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: "task-2".to_string(),
                outcome: "Implemented error handling".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![evidence.evidence_id],
                trace_parent: Some("goal:task-2".to_string()),
            }),
        })
        .expect_err("unsafe success close outcome should be rejected");

    assert!(
        err.to_string().contains("unsafe"),
        "unexpected error: {err}"
    );
}

#[test]
fn reliability_close_trace_chain_enforced() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);

    let bad_trace_contract = reliability_contract("task-trace-bad", Vec::new());
    let bad_trace_grant = state
        .request_dispatch(&bad_trace_contract, "agent-1", 60)
        .expect("dispatch");
    let bad_trace_evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-trace-bad".to_string(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("evidence");

    let synthetic_err = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-trace-bad".to_string(),
            lease_id: bad_trace_grant.lease_id,
            fence_token: bad_trace_grant.fence_token,
            patch_digest: "sha256:trace-bad".to_string(),
            verify_run_id: "vr-trace-bad".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: "task-trace-bad".to_string(),
                outcome: "Implemented trace validation".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![bad_trace_evidence.evidence_id],
                trace_parent: Some("rpc:auto".to_string()),
            }),
        })
        .expect_err("synthetic trace should be rejected");
    assert!(synthetic_err.to_string().contains("synthetic"));

    let mismatch_contract = reliability_contract("task-trace-mismatch", Vec::new());
    let mismatch_grant = state
        .request_dispatch(&mismatch_contract, "agent-2", 60)
        .expect("dispatch");
    let mismatch_evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-trace-mismatch".to_string(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("evidence");

    let mismatch_err = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-trace-mismatch".to_string(),
            lease_id: mismatch_grant.lease_id,
            fence_token: mismatch_grant.fence_token,
            patch_digest: "sha256:trace-mismatch".to_string(),
            verify_run_id: "vr-trace-mismatch".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: "task-trace-mismatch".to_string(),
                outcome: "Implemented trace validation".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![mismatch_evidence.evidence_id],
                trace_parent: Some("goal:other-task".to_string()),
            }),
        })
        .expect_err("mismatched trace should be rejected");
    assert!(mismatch_err.to_string().contains("mismatch"));

    let ok_contract = reliability_contract("task-trace-ok", Vec::new());
    let ok_grant = state
        .request_dispatch(&ok_contract, "agent-3", 60)
        .expect("dispatch");
    let ok_evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-trace-ok".to_string(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("evidence");

    let ok = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-trace-ok".to_string(),
            lease_id: ok_grant.lease_id,
            fence_token: ok_grant.fence_token,
            patch_digest: "sha256:trace-ok".to_string(),
            verify_run_id: "vr-trace-ok".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: "task-trace-ok".to_string(),
                outcome: "Implemented trace validation".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![ok_evidence.evidence_id],
                trace_parent: Some("goal:task-trace-ok".to_string()),
            }),
        })
        .expect("valid trace chain should pass");
    assert!(ok.close.approved);
}

#[test]
fn reliability_dispatch_rejects_cycle_at_dag_integration_boundary() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);

    let a = reliability_contract(
        "task-a",
        vec![TaskPrerequisite {
            task_id: "task-b".to_string(),
            trigger: reliability::EdgeTrigger::OnSuccess,
        }],
    );
    let b = reliability_contract(
        "task-b",
        vec![TaskPrerequisite {
            task_id: "task-a".to_string(),
            trigger: reliability::EdgeTrigger::OnSuccess,
        }],
    );

    let first_err = state
        .request_dispatch(&a, "agent-1", 60)
        .expect_err("task-a should be blocked waiting on task-b");
    assert!(
        first_err.to_string().contains("blocked by prerequisites"),
        "unexpected first error: {first_err}"
    );

    let cycle_err = state
        .request_dispatch(&b, "agent-1", 60)
        .expect_err("cycle must be rejected at dispatch integration boundary");
    assert!(
        cycle_err.to_string().contains("cycle"),
        "unexpected cycle error: {cycle_err}"
    );
}

#[test]
fn reliability_dispatch_promotes_blocked_task_when_prerequisite_succeeds() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let dependent = reliability_contract(
        "task-dependent",
        vec![TaskPrerequisite {
            task_id: "task-root".to_string(),
            trigger: reliability::EdgeTrigger::OnSuccess,
        }],
    );
    let root = reliability_contract("task-root", Vec::new());

    let blocked_err = state
        .request_dispatch(&dependent, "agent-1", 60)
        .expect_err("dependent should start blocked");
    assert!(
        blocked_err.to_string().contains("blocked by prerequisites"),
        "unexpected blocked error: {blocked_err}"
    );
    let blocked = state
        .tasks
        .get("task-dependent")
        .expect("dependent task exists");
    assert!(matches!(
        blocked.runtime.state,
        reliability::RuntimeState::Blocked { .. }
    ));

    let grant = state
        .request_dispatch(&root, "agent-1", 60)
        .expect("root dispatch");
    let root_evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-root".to_string(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("root evidence");
    let _ = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-root".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:root".to_string(),
            verify_run_id: "vr-root".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: "task-root".to_string(),
                outcome: "Implemented prerequisite completion".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![root_evidence.evidence_id],
                trace_parent: Some("goal:task-root".to_string()),
            }),
        })
        .expect("root submit");

    let now_ready = state
        .tasks
        .get("task-dependent")
        .expect("dependent task exists");
    assert!(matches!(
        now_ready.runtime.state,
        reliability::RuntimeState::Ready
    ));

    let dependent_grant = state
        .request_dispatch(&dependent, "agent-2", 60)
        .expect("dependent dispatch should succeed after prerequisite completion");
    assert_eq!(dependent_grant.task_id, "task-dependent");
}

#[test]
fn reliability_recovery_promotion_loop() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, false);
    let contract = reliability_contract("task-recovery", Vec::new());

    let grant = state
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");
    state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-recovery".to_string(),
            command: "cargo test".to_string(),
            exit_code: 1,
            stdout: String::new(),
            stderr: "infra transient".to_string(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("append evidence");
    let _ = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-recovery".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:retry".to_string(),
            verify_run_id: "vr-retry".to_string(),
            verify_passed: Some(false),
            verify_timed_out: false,
            failure_class: Some(reliability::FailureClass::InfraTransient),
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: None,
        })
        .expect("submit transitions to recoverable");

    let task = state.tasks.get_mut("task-recovery").expect("task exists");
    if let reliability::RuntimeState::Recoverable { retry_after, .. } = &mut task.runtime.state {
        *retry_after = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    } else {
        panic!("task must be recoverable before promotion");
    }

    let dispatch_after_retry = state
        .request_dispatch(&contract, "agent-2", 60)
        .expect("due recoverable task should auto-promote then dispatch");
    assert_eq!(dispatch_after_retry.task_id, "task-recovery");
    assert_eq!(dispatch_after_retry.state, "leased");
}

#[test]
fn reliability_open_ended_defer_requires_trigger_when_disabled() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, false);
    let contract = reliability_contract("task-defer", Vec::new());
    let grant = state
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");

    let err = state
        .resolve_blocker(BlockerReport {
            task_id: "task-defer".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            reason: "defer".to_string(),
            context: "waiting".to_string(),
            defer_trigger: None,
            resolved: false,
        })
        .expect_err("open-ended defer should be rejected");
    assert!(err.to_string().contains("defer_trigger"));
}

#[test]
fn reliability_human_needed_failure_routes_to_awaiting_human_with_next_action() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
    let contract = reliability_contract("task-human", Vec::new());
    let grant = state
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");
    let evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-human".to_string(),
            command: "cargo test".to_string(),
            exit_code: 1,
            stdout: String::new(),
            stderr: "needs operator".to_string(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("append evidence");

    let _ = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-human".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:human".to_string(),
            verify_run_id: "vr-human".to_string(),
            verify_passed: Some(false),
            verify_timed_out: false,
            failure_class: Some(reliability::FailureClass::InfraPermanent),
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: "task-human".to_string(),
                outcome: "failed: requires human intervention".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Failure),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![evidence.evidence_id],
                trace_parent: Some("goal:task-human".to_string()),
            }),
        })
        .expect("submit should route to awaiting human");

    let task = state.tasks.get("task-human").expect("task exists");
    assert!(matches!(
        task.runtime.state,
        reliability::RuntimeState::AwaitingHuman { .. }
    ));

    let digest = state
        .get_state_digest("task-human")
        .expect("digest should succeed");
    assert_eq!(digest.phase, "awaiting_human");
    assert_eq!(digest.next_action.as_deref(), Some("Resolve human blocker"));
}

#[test]
fn reliability_submit_task_enforces_scope_audit_in_hard_mode() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let mut contract = reliability_contract("task-scope", Vec::new());
    contract.max_touched_files = Some(1);
    contract.forbid_paths = vec!["src/secrets/".to_string()];
    let grant = state
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");
    let evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-scope".to_string(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("append evidence");

    let err = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-scope".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:scope".to_string(),
            verify_run_id: "vr-scope".to_string(),
            verify_passed: Some(true),
            verify_timed_out: false,
            failure_class: None,
            changed_files: vec!["src/main.rs".to_string(), "src/secrets/key.rs".to_string()],
            symbol_drift_violations: Vec::new(),
            close: Some(ClosePayload {
                task_id: "task-scope".to_string(),
                outcome: "Implemented scope policy".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![evidence.evidence_id],
                trace_parent: Some("epic-1".to_string()),
            }),
        })
        .expect_err("scope violations should reject hard mode submit");
    assert!(err.to_string().contains("touched file count"));
    assert!(err.to_string().contains("forbidden path"));
}

#[test]
fn reliability_submit_task_enforces_timeout_and_symbol_drift_in_hard_mode() {
    let mut state = reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
    let mut contract = reliability_contract("task-symbol", Vec::new());
    contract.enforce_symbol_drift_check = true;
    let grant = state
        .request_dispatch(&contract, "agent-1", 60)
        .expect("dispatch");
    let evidence = state
        .append_evidence(AppendEvidenceRequest {
            task_id: "task-symbol".to_string(),
            command: "cargo test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: None,
        })
        .expect("append evidence");

    let err = state
        .submit_task(SubmitTaskRequest {
            task_id: "task-symbol".to_string(),
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            patch_digest: "sha256:symbol".to_string(),
            verify_run_id: "vr-symbol".to_string(),
            verify_passed: Some(true),
            verify_timed_out: true,
            failure_class: None,
            changed_files: Vec::new(),
            symbol_drift_violations: vec!["public fn foo() signature changed".to_string()],
            close: Some(ClosePayload {
                task_id: "task-symbol".to_string(),
                outcome: "Implemented symbol verification".to_string(),
                outcome_kind: Some(reliability::CloseOutcomeKind::Success),
                acceptance_ids: vec!["ac-1".to_string()],
                evidence_ids: vec![evidence.evidence_id],
                trace_parent: Some("epic-1".to_string()),
            }),
        })
        .expect_err("timeout/symbol drift should reject hard mode submit");
    assert!(err.to_string().contains("timed out"));
    assert!(err.to_string().contains("symbol/API drift detected"));
}

// -----------------------------------------------------------------------
// parse_queue_mode
// -----------------------------------------------------------------------

#[test]
fn parse_queue_mode_all() {
    assert_eq!(parse_queue_mode(Some("all")), Some(QueueMode::All));
}

#[test]
fn parse_queue_mode_one_at_a_time() {
    assert_eq!(
        parse_queue_mode(Some("one-at-a-time")),
        Some(QueueMode::OneAtATime)
    );
}

#[test]
fn parse_queue_mode_none_value() {
    assert_eq!(parse_queue_mode(None), None);
}

#[test]
fn parse_queue_mode_unknown_returns_none() {
    assert_eq!(parse_queue_mode(Some("batch")), None);
    assert_eq!(parse_queue_mode(Some("")), None);
}

#[test]
fn parse_queue_mode_trims_whitespace() {
    assert_eq!(parse_queue_mode(Some("  all  ")), Some(QueueMode::All));
}

#[test]
fn provider_ids_match_accepts_aliases() {
    assert!(provider_ids_match("openrouter", "open-router"));
    assert!(provider_ids_match("google-gemini-cli", "gemini-cli"));
    assert!(!provider_ids_match("openai", "anthropic"));
}

#[test]
fn sync_agent_queue_modes_applies_steering_mode_to_live_agent() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let tools = ToolRegistry::new(&[], std::path::Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let inner_session = Arc::new(Mutex::new(Session::in_memory()));
        let session = Arc::new(Mutex::new(AgentSession::new(
            agent,
            inner_session,
            false,
            ResolvedCompactionSettings::default(),
        )));
        let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&Config::default())));
        let cx = AgentCx::for_request();

        register_rpc_queue_fetchers_for_test(&session, &shared_state, &cx).await;
        sync_agent_queue_modes(&session, &shared_state, &cx)
            .await
            .expect("initial queue mode sync");

        {
            let mut state = shared_state.lock(&cx).await.expect("state lock");
            state.steering_mode = QueueMode::All;
            state
                .push_steering(queued_user_message("steer-1"))
                .expect("queue steer-1");
            state
                .push_steering(queued_user_message("steer-2"))
                .expect("queue steer-2");
        }

        sync_agent_queue_modes(&session, &shared_state, &cx)
            .await
            .expect("update queue mode sync");

        {
            let mut guard = session.lock(&cx).await.expect("session lock");
            guard
                .run_text("prompt".to_string(), |_| {})
                .await
                .expect("run_text");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "steering all-mode should deliver both queued messages in one turn"
        );
    });
}

#[test]
fn sync_agent_queue_modes_applies_follow_up_mode_to_live_agent() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let tools = ToolRegistry::new(&[], std::path::Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let inner_session = Arc::new(Mutex::new(Session::in_memory()));
        let session = Arc::new(Mutex::new(AgentSession::new(
            agent,
            inner_session,
            false,
            ResolvedCompactionSettings::default(),
        )));
        let shared_state = Arc::new(Mutex::new(RpcSharedState::new(&Config::default())));
        let cx = AgentCx::for_request();

        register_rpc_queue_fetchers_for_test(&session, &shared_state, &cx).await;
        sync_agent_queue_modes(&session, &shared_state, &cx)
            .await
            .expect("initial queue mode sync");

        {
            let mut state = shared_state.lock(&cx).await.expect("state lock");
            state.follow_up_mode = QueueMode::All;
            state
                .push_follow_up(queued_user_message("follow-1"))
                .expect("queue follow-1");
            state
                .push_follow_up(queued_user_message("follow-2"))
                .expect("queue follow-2");
        }

        sync_agent_queue_modes(&session, &shared_state, &cx)
            .await
            .expect("update queue mode sync");

        {
            let mut guard = session.lock(&cx).await.expect("session lock");
            guard
                .run_text("prompt".to_string(), |_| {})
                .await
                .expect("run_text");
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "follow-up all-mode should batch both queued messages into one idle turn"
        );
    });
}

#[test]
fn resolve_model_key_prefers_stored_auth_key_over_inline_entry_key() {
    let mut entry = dummy_entry("gpt-4o-mini", true);
    entry.model.provider = "openai".to_string();
    entry.auth_header = true;
    entry.api_key = Some("inline-model-key".to_string());

    let auth_path = tempfile::tempdir()
        .expect("tempdir")
        .path()
        .join("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("auth load");
    auth.set(
        "openai".to_string(),
        AuthCredential::ApiKey {
            key: "stored-auth-key".to_string(),
        },
    );

    assert_eq!(
        resolve_model_key(&auth, &entry).as_deref(),
        Some("stored-auth-key")
    );
}

#[test]
fn resolve_model_key_ignores_blank_inline_key_and_falls_back_to_auth_storage() {
    let mut entry = dummy_entry("gpt-4o-mini", true);
    entry.model.provider = "openai".to_string();
    entry.auth_header = true;
    entry.api_key = Some("   ".to_string());

    let auth_path = tempfile::tempdir()
        .expect("tempdir")
        .path()
        .join("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("auth load");
    auth.set(
        "openai".to_string(),
        AuthCredential::ApiKey {
            key: "stored-auth-key".to_string(),
        },
    );

    assert_eq!(
        resolve_model_key(&auth, &entry).as_deref(),
        Some("stored-auth-key")
    );
}

#[test]
fn unknown_keyless_model_does_not_require_credentials() {
    let mut entry = dummy_entry("dev-model", false);
    entry.model.provider = "acme-local".to_string();
    entry.auth_header = false;
    entry.oauth_config = None;

    assert!(!model_requires_configured_credential(&entry));
}

#[test]
fn anthropic_model_requires_credentials_even_without_auth_header() {
    let mut entry = dummy_entry("claude-sonnet-4-6", true);
    entry.model.provider = "anthropic".to_string();
    entry.auth_header = false;
    entry.oauth_config = None;

    assert!(model_requires_configured_credential(&entry));
}

// -----------------------------------------------------------------------
// parse_streaming_behavior
// -----------------------------------------------------------------------

#[test]
fn parse_streaming_behavior_steer() {
    let val = json!("steer");
    let result = parse_streaming_behavior(Some(&val)).unwrap();
    assert_eq!(result, Some(StreamingBehavior::Steer));
}

#[test]
fn parse_streaming_behavior_follow_up_hyphenated() {
    let val = json!("follow-up");
    let result = parse_streaming_behavior(Some(&val)).unwrap();
    assert_eq!(result, Some(StreamingBehavior::FollowUp));
}

#[test]
fn parse_streaming_behavior_follow_up_camel() {
    let val = json!("followUp");
    let result = parse_streaming_behavior(Some(&val)).unwrap();
    assert_eq!(result, Some(StreamingBehavior::FollowUp));
}

#[test]
fn parse_streaming_behavior_none() {
    let result = parse_streaming_behavior(None).unwrap();
    assert_eq!(result, None);
}

#[test]
fn parse_streaming_behavior_invalid_string() {
    let val = json!("invalid");
    assert!(parse_streaming_behavior(Some(&val)).is_err());
}

#[test]
fn parse_streaming_behavior_non_string_errors() {
    let val = json!(42);
    assert!(parse_streaming_behavior(Some(&val)).is_err());
}

// -----------------------------------------------------------------------
// normalize_command_type
// -----------------------------------------------------------------------

#[test]
fn normalize_command_type_passthrough() {
    assert_eq!(normalize_command_type("prompt"), "prompt");
    assert_eq!(normalize_command_type("compact"), "compact");
}

#[test]
fn normalize_command_type_follow_up_aliases() {
    assert_eq!(normalize_command_type("follow-up"), "follow_up");
    assert_eq!(normalize_command_type("followUp"), "follow_up");
    assert_eq!(normalize_command_type("queue-follow-up"), "follow_up");
    assert_eq!(normalize_command_type("queueFollowUp"), "follow_up");
}

#[test]
fn normalize_command_type_kebab_and_camel_aliases() {
    assert_eq!(normalize_command_type("get-state"), "get_state");
    assert_eq!(normalize_command_type("getState"), "get_state");
    assert_eq!(normalize_command_type("set-model"), "set_model");
    assert_eq!(normalize_command_type("setModel"), "set_model");
    assert_eq!(
        normalize_command_type("set-steering-mode"),
        "set_steering_mode"
    );
    assert_eq!(
        normalize_command_type("setSteeringMode"),
        "set_steering_mode"
    );
    assert_eq!(
        normalize_command_type("set-follow-up-mode"),
        "set_follow_up_mode"
    );
    assert_eq!(
        normalize_command_type("setFollowUpMode"),
        "set_follow_up_mode"
    );
    assert_eq!(
        normalize_command_type("set-auto-compaction"),
        "set_auto_compaction"
    );
    assert_eq!(
        normalize_command_type("setAutoCompaction"),
        "set_auto_compaction"
    );
    assert_eq!(normalize_command_type("set-auto-retry"), "set_auto_retry");
    assert_eq!(normalize_command_type("setAutoRetry"), "set_auto_retry");
}

#[test]
fn normalize_command_type_orchestration_aliases() {
    assert_eq!(
        normalize_command_type("orchestration.startRun"),
        "orchestration.start_run"
    );
    assert_eq!(
        normalize_command_type("orchestration_start_run"),
        "orchestration.start_run"
    );
    assert_eq!(
        normalize_command_type("orchestration.getRun"),
        "orchestration.get_run"
    );
    assert_eq!(
        normalize_command_type("orchestration_get_run"),
        "orchestration.get_run"
    );
    assert_eq!(
        normalize_command_type("orchestration.dispatchRun"),
        "orchestration.dispatch_run"
    );
    assert_eq!(
        normalize_command_type("orchestration_dispatch_run"),
        "orchestration.dispatch_run"
    );
    assert_eq!(
        normalize_command_type("orchestration.cancelRun"),
        "orchestration.cancel_run"
    );
    assert_eq!(
        normalize_command_type("orchestration_cancel_run"),
        "orchestration.cancel_run"
    );
    assert_eq!(
        normalize_command_type("orchestration.resumeRun"),
        "orchestration.resume_run"
    );
    assert_eq!(
        normalize_command_type("orchestration_resume_run"),
        "orchestration.resume_run"
    );
}

// -----------------------------------------------------------------------
// build_user_message
// -----------------------------------------------------------------------

#[test]
fn build_user_message_text_only() {
    let msg = build_user_message("hello", &[]);
    match msg {
        Message::User(UserMessage {
            content: UserContent::Text(text),
            ..
        }) => assert_eq!(text, "hello"),
        other => panic!("expected text user message, got {other:?}"),
    }
}

#[test]
fn build_user_message_with_images() {
    let images = vec![ImageContent {
        data: "base64data".to_string(),
        mime_type: "image/png".to_string(),
    }];
    let msg = build_user_message("look at this", &images);
    match msg {
        Message::User(UserMessage {
            content: UserContent::Blocks(blocks),
            ..
        }) => {
            assert_eq!(blocks.len(), 2);
            assert!(matches!(&blocks[0], ContentBlock::Text(_)));
            assert!(matches!(&blocks[1], ContentBlock::Image(_)));
        }
        other => panic!("expected blocks user message, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// is_extension_command
// -----------------------------------------------------------------------

#[test]
fn is_extension_command_slash_unchanged() {
    assert!(is_extension_command("/mycommand", "/mycommand"));
}

#[test]
fn is_extension_command_expanded_returns_false() {
    // If the resource loader expanded it, the expanded text differs from the original.
    assert!(!is_extension_command(
        "/prompt-name",
        "This is the expanded prompt text."
    ));
}

#[test]
fn is_extension_command_no_slash() {
    assert!(!is_extension_command("hello", "hello"));
}

#[test]
fn is_extension_command_leading_whitespace() {
    assert!(is_extension_command("  /cmd", "  /cmd"));
}

// -----------------------------------------------------------------------
// try_send_line_with_backpressure
// -----------------------------------------------------------------------

#[test]
fn try_send_line_with_backpressure_enqueues_when_capacity_available() {
    let (tx, _rx) = mpsc::channel::<String>(1);
    assert!(try_send_line_with_backpressure(&tx, "line".to_string()));
    assert!(matches!(
        tx.try_send("next".to_string()),
        Err(mpsc::SendError::Full(_))
    ));
}

#[test]
fn try_send_line_with_backpressure_stops_when_receiver_closed() {
    let (tx, rx) = mpsc::channel::<String>(1);
    drop(rx);
    assert!(!try_send_line_with_backpressure(&tx, "line".to_string()));
}

#[test]
fn try_send_line_with_backpressure_waits_until_capacity_is_available() {
    let (tx, rx) = mpsc::channel::<String>(1);
    tx.try_send("occupied".to_string())
        .expect("seed initial occupied slot");

    let expected = "delayed-line".to_string();
    let expected_for_thread = expected.clone();
    let recv_handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut received = Vec::new();
        while received.len() < 2 && Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                received.push(msg);
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(received.len(), 2, "should receive both queued lines");
        let first = received.remove(0);
        let second = received.remove(0);
        assert_eq!(first, "occupied");
        assert_eq!(second, expected_for_thread);
    });

    assert!(try_send_line_with_backpressure(&tx, expected));
    drop(tx);
    recv_handle.join().expect("receiver thread should finish");
}

#[test]
fn try_send_line_with_backpressure_preserves_large_payload() {
    let (tx, rx) = mpsc::channel::<String>(1);
    tx.try_send("busy".to_string())
        .expect("seed initial busy slot");

    let large = "x".repeat(256 * 1024);
    let large_for_thread = large.clone();
    let recv_handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut received = Vec::new();
        while received.len() < 2 && Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                received.push(msg);
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(received.len(), 2, "should receive busy + payload lines");
        let payload = received.remove(1);
        assert_eq!(payload.len(), large_for_thread.len());
        assert_eq!(payload, large_for_thread);
    });

    assert!(try_send_line_with_backpressure(&tx, large));
    drop(tx);
    recv_handle.join().expect("receiver thread should finish");
}

#[test]
fn try_send_line_with_backpressure_detects_disconnect_while_waiting() {
    let (tx, rx) = mpsc::channel::<String>(1);
    tx.try_send("busy".to_string())
        .expect("seed initial busy slot");

    let drop_handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        drop(rx);
    });

    assert!(
        !try_send_line_with_backpressure(&tx, "line-after-disconnect".to_string()),
        "send should stop after receiver disconnects while channel is full"
    );
    drop_handle.join().expect("drop thread should finish");
}

#[test]
fn try_send_line_with_backpressure_high_volume_preserves_order_and_count() {
    let (tx, rx) = mpsc::channel::<String>(4);
    let lines: Vec<String> = (0..256)
        .map(|idx| format!("line-{idx:03}: {}", "x".repeat(64)))
        .collect();
    let expected = lines.clone();

    let recv_handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut received = Vec::new();
        while received.len() < expected.len() && Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                received.push(msg);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            received.len(),
            expected.len(),
            "should receive every line under sustained backpressure"
        );
        assert_eq!(received, expected, "line ordering must remain stable");
    });

    for line in lines {
        assert!(try_send_line_with_backpressure(&tx, line));
    }
    drop(tx);
    recv_handle.join().expect("receiver thread should finish");
}

#[test]
fn try_send_line_with_backpressure_preserves_partial_line_without_newline() {
    let (tx, rx) = mpsc::channel::<String>(1);
    tx.try_send("busy".to_string())
        .expect("seed initial busy slot");

    let partial_json = "{\"type\":\"prompt\",\"message\":\"tail-fragment-ascii\"".to_string();
    let expected = partial_json.clone();

    let recv_handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(25));
        let first = rx.try_recv().expect("seeded line should be available");
        assert_eq!(first, "busy");
        let deadline = Instant::now() + Duration::from_millis(500);
        let second = loop {
            if let Ok(line) = rx.try_recv() {
                break line;
            }
            assert!(
                Instant::now() < deadline,
                "partial payload should be available"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(second, expected);
    });

    assert!(try_send_line_with_backpressure(&tx, partial_json));
    drop(tx);
    recv_handle.join().expect("receiver thread should finish");
}

// -----------------------------------------------------------------------
// RpcStateSnapshot::pending_count
// -----------------------------------------------------------------------

#[test]
fn snapshot_pending_count() {
    let snapshot = RpcStateSnapshot {
        steering_count: 3,
        follow_up_count: 7,
        steering_mode: QueueMode::All,
        follow_up_mode: QueueMode::OneAtATime,
        auto_compaction_enabled: false,
        auto_retry_enabled: true,
    };
    assert_eq!(snapshot.pending_count(), 10);
}

#[test]
fn snapshot_pending_count_zero() {
    let snapshot = RpcStateSnapshot {
        steering_count: 0,
        follow_up_count: 0,
        steering_mode: QueueMode::All,
        follow_up_mode: QueueMode::All,
        auto_compaction_enabled: false,
        auto_retry_enabled: false,
    };
    assert_eq!(snapshot.pending_count(), 0);
}

// -----------------------------------------------------------------------
// retry_delay_ms
// -----------------------------------------------------------------------

#[test]
fn retry_delay_first_attempt_is_base() {
    let config = Config::default();
    // attempt 0 and 1 should both use the base delay (shift = attempt - 1 saturating)
    assert_eq!(retry_delay_ms(&config, 0), config.retry_base_delay_ms());
    assert_eq!(retry_delay_ms(&config, 1), config.retry_base_delay_ms());
}

#[test]
fn retry_delay_doubles_each_attempt() {
    let config = Config::default();
    let base = config.retry_base_delay_ms();
    // attempt 2: base * 2, attempt 3: base * 4
    assert_eq!(retry_delay_ms(&config, 2), base * 2);
    assert_eq!(retry_delay_ms(&config, 3), base * 4);
}

#[test]
fn retry_delay_capped_at_max() {
    let config = Config::default();
    let max = config.retry_max_delay_ms();
    // Large attempt number should be capped
    let delay = retry_delay_ms(&config, 30);
    assert_eq!(delay, max);
}

#[test]
fn retry_delay_saturates_on_overflow() {
    let config = Config::default();
    // u32::MAX attempt should not panic
    let delay = retry_delay_ms(&config, u32::MAX);
    assert!(delay <= config.retry_max_delay_ms());
}

// -----------------------------------------------------------------------
// should_auto_compact
// -----------------------------------------------------------------------

#[test]
fn auto_compact_below_threshold() {
    // 50k tokens used, 200k window, 40k reserve → threshold = 160k → no compact
    assert!(!should_auto_compact(50_000, 200_000, 40_000));
}

#[test]
fn auto_compact_above_threshold() {
    // 170k tokens used, 200k window, 40k reserve → threshold = 160k → compact
    assert!(should_auto_compact(170_000, 200_000, 40_000));
}

#[test]
fn auto_compact_exact_threshold() {
    // Exactly at threshold → not above → no compact
    assert!(!should_auto_compact(160_000, 200_000, 40_000));
}

#[test]
fn auto_compact_reserve_exceeds_window() {
    // reserve > window → window - reserve saturates to 0 → any tokens > 0 triggers compact
    assert!(should_auto_compact(1, 100, 200));
}

#[test]
fn auto_compact_zero_tokens() {
    assert!(!should_auto_compact(0, 200_000, 40_000));
}

// -----------------------------------------------------------------------
// rpc_flatten_content_blocks
// -----------------------------------------------------------------------

#[test]
fn flatten_content_blocks_unwraps_inner_0() {
    let mut value = json!({
        "content": [
            {"0": {"type": "text", "text": "hello"}}
        ]
    });
    rpc_flatten_content_blocks(&mut value);
    let blocks = value["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "hello");
    assert!(blocks[0].get("0").is_none());
}

#[test]
fn flatten_content_blocks_preserves_non_wrapped() {
    let mut value = json!({
        "content": [
            {"type": "text", "text": "already flat"}
        ]
    });
    rpc_flatten_content_blocks(&mut value);
    let blocks = value["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "already flat");
}

#[test]
fn flatten_content_blocks_no_content_field() {
    let mut value = json!({"role": "assistant"});
    rpc_flatten_content_blocks(&mut value); // should not panic
    assert_eq!(value, json!({"role": "assistant"}));
}

#[test]
fn flatten_content_blocks_non_object() {
    let mut value = json!("just a string");
    rpc_flatten_content_blocks(&mut value); // should not panic
}

#[test]
fn flatten_content_blocks_existing_keys_not_overwritten() {
    // If a block already has a key that conflicts with inner "0", preserve outer
    let mut value = json!({
        "content": [
            {"type": "existing", "0": {"type": "inner", "extra": "data"}}
        ]
    });
    rpc_flatten_content_blocks(&mut value);
    let blocks = value["content"].as_array().unwrap();
    // "type" should keep the outer "existing" value, not be overwritten by inner "inner"
    assert_eq!(blocks[0]["type"], "existing");
    // "extra" from inner should be merged in
    assert_eq!(blocks[0]["extra"], "data");
}

// -----------------------------------------------------------------------
// parse_prompt_images
// -----------------------------------------------------------------------

#[test]
fn parse_prompt_images_none() {
    let images = parse_prompt_images(None).unwrap();
    assert!(images.is_empty());
}

#[test]
fn parse_prompt_images_empty_array() {
    let val = json!([]);
    let images = parse_prompt_images(Some(&val)).unwrap();
    assert!(images.is_empty());
}

#[test]
fn parse_prompt_images_valid() {
    let val = json!([{
        "type": "image",
        "source": {
            "type": "base64",
            "mediaType": "image/png",
            "data": "iVBORw0KGgo="
        }
    }]);
    let images = parse_prompt_images(Some(&val)).unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(images[0].data, "iVBORw0KGgo=");
}

#[test]
fn parse_prompt_images_skips_non_image_type() {
    let val = json!([{
        "type": "text",
        "text": "hello"
    }]);
    let images = parse_prompt_images(Some(&val)).unwrap();
    assert!(images.is_empty());
}

#[test]
fn parse_prompt_images_skips_non_base64_source() {
    let val = json!([{
        "type": "image",
        "source": {
            "type": "url",
            "url": "https://example.com/img.png"
        }
    }]);
    let images = parse_prompt_images(Some(&val)).unwrap();
    assert!(images.is_empty());
}

#[test]
fn parse_prompt_images_not_array_errors() {
    let val = json!("not-an-array");
    assert!(parse_prompt_images(Some(&val)).is_err());
}

#[test]
fn parse_prompt_images_multiple_valid() {
    let val = json!([
        {
            "type": "image",
            "source": {"type": "base64", "mediaType": "image/jpeg", "data": "abc"}
        },
        {
            "type": "image",
            "source": {"type": "base64", "mediaType": "image/webp", "data": "def"}
        }
    ]);
    let images = parse_prompt_images(Some(&val)).unwrap();
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].mime_type, "image/jpeg");
    assert_eq!(images[1].mime_type, "image/webp");
}

// -----------------------------------------------------------------------
// extract_user_text
// -----------------------------------------------------------------------

#[test]
fn extract_user_text_from_text_content() {
    let content = UserContent::Text("hello world".to_string());
    assert_eq!(extract_user_text(&content), Some("hello world".to_string()));
}

#[test]
fn extract_user_text_from_blocks() {
    let content = UserContent::Blocks(vec![
        ContentBlock::Image(ImageContent {
            data: String::new(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Text(TextContent::new("found it")),
    ]);
    assert_eq!(extract_user_text(&content), Some("found it".to_string()));
}

#[test]
fn extract_user_text_blocks_no_text() {
    let content = UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
        data: String::new(),
        mime_type: "image/png".to_string(),
    })]);
    assert_eq!(extract_user_text(&content), None);
}

// -----------------------------------------------------------------------
// parse_thinking_level
// -----------------------------------------------------------------------

#[test]
fn parse_thinking_level_all_variants() {
    assert_eq!(parse_thinking_level("off").unwrap(), ThinkingLevel::Off);
    assert_eq!(parse_thinking_level("none").unwrap(), ThinkingLevel::Off);
    assert_eq!(parse_thinking_level("0").unwrap(), ThinkingLevel::Off);
    assert_eq!(
        parse_thinking_level("minimal").unwrap(),
        ThinkingLevel::Minimal
    );
    assert_eq!(parse_thinking_level("min").unwrap(), ThinkingLevel::Minimal);
    assert_eq!(parse_thinking_level("low").unwrap(), ThinkingLevel::Low);
    assert_eq!(parse_thinking_level("1").unwrap(), ThinkingLevel::Low);
    assert_eq!(
        parse_thinking_level("medium").unwrap(),
        ThinkingLevel::Medium
    );
    assert_eq!(parse_thinking_level("med").unwrap(), ThinkingLevel::Medium);
    assert_eq!(parse_thinking_level("2").unwrap(), ThinkingLevel::Medium);
    assert_eq!(parse_thinking_level("high").unwrap(), ThinkingLevel::High);
    assert_eq!(parse_thinking_level("3").unwrap(), ThinkingLevel::High);
    assert_eq!(parse_thinking_level("xhigh").unwrap(), ThinkingLevel::XHigh);
    assert_eq!(parse_thinking_level("4").unwrap(), ThinkingLevel::XHigh);
}

#[test]
fn parse_thinking_level_case_insensitive() {
    assert_eq!(parse_thinking_level("HIGH").unwrap(), ThinkingLevel::High);
    assert_eq!(
        parse_thinking_level("Medium").unwrap(),
        ThinkingLevel::Medium
    );
    assert_eq!(parse_thinking_level("  Off  ").unwrap(), ThinkingLevel::Off);
}

#[test]
fn parse_thinking_level_invalid() {
    assert!(parse_thinking_level("invalid").is_err());
    assert!(parse_thinking_level("").is_err());
    assert!(parse_thinking_level("5").is_err());
}

// -----------------------------------------------------------------------
// supports_xhigh + clamp_thinking_level
// -----------------------------------------------------------------------

#[test]
fn supports_xhigh_known_models() {
    assert!(dummy_entry("gpt-5.1-codex-max", true).supports_xhigh());
    assert!(dummy_entry("gpt-5.2", true).supports_xhigh());
    assert!(dummy_entry("gpt-5.2-codex", true).supports_xhigh());
    assert!(dummy_entry("gpt-5.3-codex", true).supports_xhigh());
}

#[test]
fn supports_xhigh_unknown_models() {
    assert!(!dummy_entry("claude-opus-4-6", true).supports_xhigh());
    assert!(!dummy_entry("gpt-4o", true).supports_xhigh());
    assert!(!dummy_entry("", true).supports_xhigh());
}

#[test]
fn clamp_thinking_non_reasoning_model() {
    let entry = dummy_entry("claude-3-haiku", false);
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::High),
        ThinkingLevel::Off
    );
}

#[test]
fn clamp_thinking_xhigh_without_support() {
    let entry = dummy_entry("claude-opus-4-6", true);
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::XHigh),
        ThinkingLevel::High
    );
}

#[test]
fn clamp_thinking_xhigh_with_support() {
    let entry = dummy_entry("gpt-5.2", true);
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::XHigh),
        ThinkingLevel::XHigh
    );
}

#[test]
fn clamp_thinking_normal_level_passthrough() {
    let entry = dummy_entry("claude-opus-4-6", true);
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::Medium),
        ThinkingLevel::Medium
    );
}

// -----------------------------------------------------------------------
// available_thinking_levels
// -----------------------------------------------------------------------

#[test]
fn available_thinking_levels_non_reasoning() {
    let entry = dummy_entry("gpt-4o-mini", false);
    let levels = available_thinking_levels(&entry);
    assert_eq!(levels, vec![ThinkingLevel::Off]);
}

#[test]
fn available_thinking_levels_reasoning_no_xhigh() {
    let entry = dummy_entry("claude-opus-4-6", true);
    let levels = available_thinking_levels(&entry);
    assert_eq!(
        levels,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ]
    );
}

#[test]
fn available_thinking_levels_reasoning_with_xhigh() {
    let entry = dummy_entry("gpt-5.2", true);
    let levels = available_thinking_levels(&entry);
    assert_eq!(
        levels,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ]
    );
}

// -----------------------------------------------------------------------
// rpc_model_from_entry
// -----------------------------------------------------------------------

#[test]
fn rpc_model_from_entry_basic() {
    let entry = dummy_entry("claude-opus-4-6", true);
    let value = rpc_model_from_entry(&entry);
    assert_eq!(value["id"], "claude-opus-4-6");
    assert_eq!(value["name"], "claude-opus-4-6");
    assert_eq!(value["provider"], "anthropic");
    assert_eq!(value["reasoning"], true);
    assert_eq!(value["contextWindow"], 200_000);
    assert_eq!(value["maxTokens"], 8192);
}

#[test]
fn rpc_model_from_entry_input_types() {
    let mut entry = dummy_entry("gpt-4o", false);
    entry.model.input = vec![InputType::Text, InputType::Image];
    let value = rpc_model_from_entry(&entry);
    let input = value["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0], "text");
    assert_eq!(input[1], "image");
}

#[test]
fn rpc_model_from_entry_cost_present() {
    let entry = dummy_entry("test-model", false);
    let value = rpc_model_from_entry(&entry);
    assert!(value.get("cost").is_some());
    let cost = &value["cost"];
    assert_eq!(cost["input"], 3.0);
    assert_eq!(cost["output"], 15.0);
}

#[test]
fn current_model_entry_matches_provider_alias_and_model_case() {
    let mut model = dummy_entry("gpt-4o-mini", true);
    model.model.provider = "openrouter".to_string();
    let options = rpc_options_with_models(vec![model]);

    let mut session = Session::in_memory();
    session.header.provider = Some("open-router".to_string());
    session.header.model_id = Some("GPT-4O-MINI".to_string());

    let resolved = current_model_entry(&session, &options).expect("resolve aliased model");
    assert_eq!(resolved.model.provider, "openrouter");
    assert_eq!(resolved.model.id, "gpt-4o-mini");
}

#[test]
fn session_state_resolves_model_for_provider_alias() {
    let mut model = dummy_entry("gpt-4o-mini", true);
    model.model.provider = "openrouter".to_string();
    let options = rpc_options_with_models(vec![model]);

    let mut session = Session::in_memory();
    session.header.provider = Some("open-router".to_string());
    session.header.model_id = Some("gpt-4o-mini".to_string());

    let snapshot = RpcStateSnapshot {
        steering_count: 0,
        follow_up_count: 0,
        steering_mode: QueueMode::OneAtATime,
        follow_up_mode: QueueMode::OneAtATime,
        auto_compaction_enabled: false,
        auto_retry_enabled: false,
    };

    let state = session_state(&session, &options, &snapshot, false, false);
    assert_eq!(state["model"]["provider"], "openrouter");
    assert_eq!(state["model"]["id"], "gpt-4o-mini");
}

// -----------------------------------------------------------------------
// error_hints_value
// -----------------------------------------------------------------------

#[test]
fn error_hints_value_produces_expected_shape() {
    let error = Error::validation("test error");
    let value = error_hints_value(&error);
    assert!(value.get("summary").is_some());
    assert!(value.get("hints").is_some());
    assert!(value.get("contextFields").is_some());
    assert!(value["hints"].is_array());
}

// -----------------------------------------------------------------------
// rpc_parse_extension_ui_response_id edge cases
// -----------------------------------------------------------------------

#[test]
fn parse_ui_response_id_empty_string() {
    let value = json!({"requestId": ""});
    assert_eq!(rpc_parse_extension_ui_response_id(&value), None);
}

#[test]
fn parse_ui_response_id_whitespace_only() {
    let value = json!({"requestId": "   "});
    assert_eq!(rpc_parse_extension_ui_response_id(&value), None);
}

#[test]
fn parse_ui_response_id_trims() {
    let value = json!({"requestId": "  req-1  "});
    assert_eq!(
        rpc_parse_extension_ui_response_id(&value),
        Some("req-1".to_string())
    );
}

#[test]
fn parse_ui_response_id_prefers_request_id_over_id_alias() {
    let value = json!({"requestId": "req-1", "id": "legacy-id"});
    assert_eq!(
        rpc_parse_extension_ui_response_id(&value),
        Some("req-1".to_string())
    );
}

#[test]
fn parse_ui_response_id_falls_back_to_id_alias_when_request_id_not_string() {
    let value = json!({"requestId": 123, "id": "legacy-id"});
    assert_eq!(
        rpc_parse_extension_ui_response_id(&value),
        Some("legacy-id".to_string())
    );
}

#[test]
fn parse_ui_response_id_falls_back_to_id_alias_when_request_id_blank() {
    let value = json!({"requestId": "", "id": "legacy-id"});
    assert_eq!(
        rpc_parse_extension_ui_response_id(&value),
        Some("legacy-id".to_string())
    );
}

#[test]
fn parse_ui_response_id_falls_back_to_id_alias_when_request_id_whitespace() {
    let value = json!({"requestId": "   ", "id": "legacy-id"});
    assert_eq!(
        rpc_parse_extension_ui_response_id(&value),
        Some("legacy-id".to_string())
    );
}

#[test]
fn parse_ui_response_id_neither_field() {
    let value = json!({"type": "something"});
    assert_eq!(rpc_parse_extension_ui_response_id(&value), None);
}

// -----------------------------------------------------------------------
// rpc_parse_extension_ui_response edge cases
// -----------------------------------------------------------------------

#[test]
fn parse_editor_response_requires_string() {
    let active = ExtensionUiRequest::new("req-1", "editor", json!({"title": "t"}));
    let ok = json!({"type": "extension_ui_response", "requestId": "req-1", "value": "code"});
    assert!(rpc_parse_extension_ui_response(&ok, &active).is_ok());

    let bad = json!({"type": "extension_ui_response", "requestId": "req-1", "value": 42});
    assert!(rpc_parse_extension_ui_response(&bad, &active).is_err());
}

#[test]
fn parse_notify_response_returns_ack() {
    let active = ExtensionUiRequest::new("req-1", "notify", json!({"title": "t"}));
    let val = json!({"type": "extension_ui_response", "requestId": "req-1"});
    let resp = rpc_parse_extension_ui_response(&val, &active).unwrap();
    assert!(!resp.cancelled);
}

#[test]
fn parse_unknown_method_errors() {
    let active = ExtensionUiRequest::new("req-1", "unknown_method", json!({}));
    let val = json!({"type": "extension_ui_response", "requestId": "req-1"});
    assert!(rpc_parse_extension_ui_response(&val, &active).is_err());
}

#[test]
fn parse_select_with_object_options() {
    let active = ExtensionUiRequest::new(
        "req-1",
        "select",
        json!({"title": "pick", "options": [{"label": "Alpha", "value": "a"}, {"label": "Beta"}]}),
    );
    // Selecting by value key
    let val_a = json!({"type": "extension_ui_response", "requestId": "req-1", "value": "a"});
    let resp = rpc_parse_extension_ui_response(&val_a, &active).unwrap();
    assert_eq!(resp.value, Some(json!("a")));

    // Selecting by label fallback (no value key in option)
    let val_b = json!({"type": "extension_ui_response", "requestId": "req-1", "value": "Beta"});
    let resp = rpc_parse_extension_ui_response(&val_b, &active).unwrap();
    assert_eq!(resp.value, Some(json!("Beta")));
}
