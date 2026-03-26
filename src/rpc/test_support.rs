use super::*;
use crate::agent::AgentSession;
use crate::extensions::ExtensionUiRequest;
use crate::services::run_service::OrchestrationInlineWorker;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) fn ensure_run_id_available(
    orchestration: &RpcOrchestrationState,
    run_store: &RunStore,
    run_id: &str,
) -> Result<()> {
    if orchestration.get_run(run_id).is_some() || run_store.exists(run_id) {
        return Err(Error::validation(format!(
            "orchestration run already exists: {run_id}"
        )));
    }
    Ok(())
}

fn orchestration_dag_depth(tasks: &[TaskContract]) -> usize {
    fn visit(
        task_id: &str,
        prereqs: &HashMap<String, Vec<String>>,
        memo: &mut HashMap<String, usize>,
        visiting: &mut HashSet<String>,
    ) -> usize {
        if let Some(depth) = memo.get(task_id) {
            return *depth;
        }
        if !visiting.insert(task_id.to_string()) {
            return 1;
        }
        let depth = prereqs
            .get(task_id)
            .into_iter()
            .flatten()
            .map(|dep| 1 + visit(dep, prereqs, memo, visiting))
            .max()
            .unwrap_or(1);
        visiting.remove(task_id);
        memo.insert(task_id.to_string(), depth);
        depth
    }

    let prereqs = tasks
        .iter()
        .map(|task| {
            (
                task.task_id.clone(),
                task.prerequisites
                    .iter()
                    .map(|dep| dep.task_id.clone())
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    tasks
        .iter()
        .map(|task| visit(&task.task_id, &prereqs, &mut memo, &mut visiting))
        .max()
        .unwrap_or(0)
}

pub(super) fn select_execution_tier(tasks: &[TaskContract]) -> ExecutionTier {
    match tasks.len() {
        0 | 1 => ExecutionTier::Inline,
        2..=24 if orchestration_dag_depth(tasks) <= 4 => ExecutionTier::Wave,
        _ => ExecutionTier::Hierarchical,
    }
}

pub mod run_service_test_helpers {
    use super::*;

    pub fn task_terminal_success(reliability: &RpcReliabilityState, task_id: &str) -> bool {
        run_service::task_terminal_success(reliability, task_id)
    }

    pub fn run_terminal_success(reliability: &RpcReliabilityState, run: &RunStatus) -> bool {
        run_service::run_terminal_success(reliability, run)
    }

    pub fn completed_run_verify_scope(
        reliability: &RpcReliabilityState,
        run: &RunStatus,
    ) -> Option<CompletedRunVerifyScope> {
        run_service::completed_run_verify_scope(reliability, run)
    }

    pub fn completed_scope_from_run_verify(status: &RunVerifyStatus) -> CompletedRunVerifyScope {
        run_service::completed_scope_from_run_verify(status)
    }

    pub fn next_active_wave(
        existing: Option<WaveStatus>,
        active_task_ids: Vec<String>,
    ) -> Option<WaveStatus> {
        run_service::next_active_wave(existing, active_task_ids)
    }

    pub async fn persist_run_status(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        run_store: &RunStore,
        status: &RunStatus,
    ) -> Result<()> {
        run_service::persist_run_status(cx, session, run_store, status).await
    }

    pub async fn append_dispatch_grants_session_entries(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        grants: &[DispatchGrant],
    ) -> Result<()> {
        run_service::append_dispatch_grants_session_entries(cx, session, reliability_state, grants)
            .await
    }

    pub async fn append_canceled_dispatch_grants_session_entries(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        grants: &[DispatchGrant],
    ) -> Result<()> {
        run_service::append_canceled_dispatch_grants_session_entries(
            cx,
            session,
            reliability_state,
            grants,
        )
        .await
    }

    pub async fn append_dispatch_rollback_session_entry(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        grant: &DispatchGrant,
        to_state: &str,
        summary: &str,
    ) -> Result<()> {
        run_service::append_dispatch_rollback_session_entry(cx, session, grant, to_state, summary)
            .await
    }

    pub fn dispatch_workspace_segment_id(grant: &DispatchGrant) -> usize {
        run_service::dispatch_workspace_segment_id(grant)
    }

    pub async fn execute_inline_dispatch_grant_with_worker(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        repo_root: &Path,
        run_id: &str,
        grant: &DispatchGrant,
        worker: &dyn OrchestrationInlineWorker,
    ) -> Result<RunStatus> {
        run_service::execute_inline_dispatch_grant_with_worker(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            repo_root,
            run_id,
            grant,
            worker,
        )
        .await
    }

    pub async fn execute_dispatch_grants_with_worker(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        repo_root: &Path,
        run: RunStatus,
        grants: &[DispatchGrant],
        worker: &dyn OrchestrationInlineWorker,
    ) -> Result<RunStatus> {
        run_service::execute_dispatch_grants_with_worker(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            repo_root,
            run,
            grants,
            worker,
        )
        .await
    }

    pub fn topological_run_task_ids(
        reliability: &RpcReliabilityState,
        run: &RunStatus,
    ) -> Vec<String> {
        run_service::topological_run_task_ids(reliability, run)
    }

    pub fn planned_subruns(reliability: &RpcReliabilityState, run: &RunStatus) -> Vec<SubrunPlan> {
        run_service::planned_subruns(reliability, run)
    }

    pub fn derive_run_lifecycle(
        reliability: &RpcReliabilityState,
        task_ids: &[String],
    ) -> RunLifecycle {
        run_service::derive_run_lifecycle(reliability, task_ids)
    }

    pub fn run_has_live_tasks(reliability: &RpcReliabilityState, run: &RunStatus) -> bool {
        run_service::run_has_live_tasks(reliability, run)
    }

    pub fn refresh_live_run_from_reliability(
        reliability: &mut RpcReliabilityState,
        run: &mut RunStatus,
    ) -> bool {
        run_service::refresh_live_run_from_reliability(reliability, run)
    }

    pub fn refresh_run_from_reliability(reliability: &RpcReliabilityState, run: &mut RunStatus) {
        run_service::refresh_run_from_reliability(reliability, run);
    }

    pub fn dispatch_run_wave(
        reliability: &mut RpcReliabilityState,
        run: &mut RunStatus,
        agent_id_prefix: &str,
        lease_ttl_sec: i64,
    ) -> Result<Vec<DispatchGrant>> {
        run_service::dispatch_run_wave(reliability, run, agent_id_prefix, lease_ttl_sec)
    }

    pub fn cancel_live_run_tasks(
        reliability: &mut RpcReliabilityState,
        run: &mut RunStatus,
    ) -> Vec<DispatchGrant> {
        run_service::cancel_live_run_tasks(reliability, run)
    }

    pub fn build_cancel_run_task_report(
        reliability: &RpcReliabilityState,
        run_id: &str,
        task_id: &str,
    ) -> Option<TaskReport> {
        run_service::build_cancel_run_task_report(reliability, run_id, task_id)
    }

    pub fn build_dispatch_rollback_task_report(
        reliability: &RpcReliabilityState,
        task_id: &str,
        summary: &str,
        failure_class: &str,
    ) -> Option<TaskReport> {
        run_service::build_dispatch_rollback_task_report(
            reliability,
            task_id,
            summary,
            failure_class,
        )
    }

    pub async fn cancel_live_run_tasks_and_sync(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        run: &mut RunStatus,
    ) -> Result<()> {
        run_service::cancel_live_run_tasks_and_sync(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            run,
        )
        .await
    }

    pub fn next_recoverable_retry_delay(
        reliability: &RpcReliabilityState,
        run: &RunStatus,
    ) -> Option<Duration> {
        run_service::next_recoverable_retry_delay(reliability, run)
    }

    pub fn apply_dispatch_rollback_recovery(
        reliability: &mut RpcReliabilityState,
        grant: &DispatchGrant,
        failure_summary: Option<&str>,
    ) -> String {
        run_service::apply_dispatch_rollback_recovery(reliability, grant, failure_summary)
    }

    pub async fn rollback_dispatch_grant(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        run_id: &str,
        grant: &DispatchGrant,
        failure_summary: Option<String>,
    ) {
        run_service::rollback_dispatch_grant(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            run_id,
            grant,
            failure_summary,
        )
        .await;
    }

    pub async fn execute_inline_run_dispatch(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        config: &Config,
        run: RunStatus,
        grants: &[DispatchGrant],
    ) -> Result<RunStatus> {
        run_service::execute_inline_run_dispatch(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            config,
            run,
            grants,
        )
        .await
    }

    pub async fn current_run_status(
        cx: &AgentCx,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        run_id: &str,
    ) -> Result<RunStatus> {
        run_service::current_run_status(cx, orchestration_state, run_store, run_id).await
    }

    pub async fn session_workspace_root(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
    ) -> Result<PathBuf> {
        run_service::session_workspace_root(cx, session).await
    }

    pub async fn current_session_identity(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
    ) -> Result<SessionIdentity> {
        run_service::current_session_identity(cx, session).await
    }

    pub async fn dispatch_run_until_quiescent(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        config: &Config,
        run: RunStatus,
        agent_id_prefix: &str,
        lease_ttl_sec: i64,
    ) -> Result<(RunStatus, Vec<DispatchGrant>)> {
        run_service::dispatch_run_until_quiescent(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            config,
            run,
            agent_id_prefix,
            lease_ttl_sec,
        )
        .await
    }

    pub fn build_submit_task_report(
        reliability: &RpcReliabilityState,
        req: &SubmitTaskRequest,
        result: &SubmitTaskResponse,
    ) -> Result<TaskReport> {
        run_service::build_submit_task_report(reliability, req, result)
    }

    pub fn build_runtime_task_report(
        reliability: &RpcReliabilityState,
        task_id: &str,
        summary: String,
    ) -> Option<TaskReport> {
        run_service::build_runtime_task_report(reliability, task_id, summary)
    }

    pub fn refresh_task_runs_with_verify_scopes(
        reliability: &RpcReliabilityState,
        orchestration: &mut RpcOrchestrationState,
        task_id: &str,
        report: Option<&TaskReport>,
    ) -> Vec<(RunStatus, Option<CompletedRunVerifyScope>)> {
        run_service::refresh_task_runs_with_verify_scopes(
            reliability,
            orchestration,
            task_id,
            report,
        )
    }

    pub fn refresh_task_runs(
        reliability: &RpcReliabilityState,
        orchestration: &mut RpcOrchestrationState,
        task_id: &str,
        report: Option<TaskReport>,
    ) -> Vec<RunStatus> {
        run_service::refresh_task_runs(reliability, orchestration, task_id, report)
    }

    pub fn run_verify_scope_summary(
        scope: &CompletedRunVerifyScope,
        ok: bool,
        details: impl AsRef<str>,
    ) -> String {
        run_service::run_verify_scope_summary(scope, ok, details)
    }

    pub async fn execute_run_verification(
        cwd: &Path,
        run: &mut RunStatus,
        scope: &CompletedRunVerifyScope,
    ) {
        run_service::execute_run_verification(cwd, run, scope).await;
    }

    pub async fn sync_task_runs(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        task_id: &str,
        report: Option<TaskReport>,
    ) -> Result<Vec<RunStatus>> {
        run_service::sync_task_runs(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            task_id,
            report,
        )
        .await
    }

    pub async fn refresh_run_if_live(
        cx: &AgentCx,
        session: &Arc<Mutex<AgentSession>>,
        reliability_state: &Arc<Mutex<RpcReliabilityState>>,
        orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
        run_store: &RunStore,
        run: RunStatus,
    ) -> Result<RunStatus> {
        run_service::refresh_run_if_live(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            run,
        )
        .await
    }
}

mod ui_bridge_tests {
    use super::*;

    #[test]
    fn parse_extension_ui_response_id_prefers_request_id() {
        let value = json!({"type":"extension_ui_response","id":"legacy","requestId":"canonical"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("canonical".to_string())
        );
    }

    #[test]
    fn parse_extension_ui_response_id_accepts_id_alias() {
        let value = json!({"type":"extension_ui_response","id":"legacy"});
        assert_eq!(
            rpc_parse_extension_ui_response_id(&value),
            Some("legacy".to_string())
        );
    }

    #[test]
    fn parse_confirm_response_accepts_confirmed_alias() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let value = json!({"type":"extension_ui_response","requestId":"req-1","confirmed":true});
        let resp = rpc_parse_extension_ui_response(&value, &active).expect("parse confirm");
        assert!(!resp.cancelled);
        assert_eq!(resp.value, Some(json!(true)));
    }

    #[test]
    fn parse_confirm_response_accepts_value_bool() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let value = json!({"type":"extension_ui_response","requestId":"req-1","value":false});
        let resp = rpc_parse_extension_ui_response(&value, &active).expect("parse confirm");
        assert!(!resp.cancelled);
        assert_eq!(resp.value, Some(json!(false)));
    }

    #[test]
    fn parse_cancelled_response_wins_over_value() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let value = json!({"type":"extension_ui_response","requestId":"req-1","cancelled":true,"value":true});
        let resp = rpc_parse_extension_ui_response(&value, &active).expect("parse cancel");
        assert!(resp.cancelled);
        assert_eq!(resp.value, None);
    }

    #[test]
    fn parse_select_response_validates_against_options() {
        let active = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({"title":"pick","options":["A","B"]}),
        );
        let ok_value = json!({"type":"extension_ui_response","requestId":"req-1","value":"B"});
        let ok = rpc_parse_extension_ui_response(&ok_value, &active).expect("parse select ok");
        assert_eq!(ok.value, Some(json!("B")));

        let bad_value = json!({"type":"extension_ui_response","requestId":"req-1","value":"C"});
        assert!(
            rpc_parse_extension_ui_response(&bad_value, &active).is_err(),
            "invalid selection should error"
        );
    }

    #[test]
    fn parse_input_requires_string_value() {
        let active = ExtensionUiRequest::new("req-1", "input", json!({"title":"t"}));
        let ok_value = json!({"type":"extension_ui_response","requestId":"req-1","value":"hi"});
        let ok = rpc_parse_extension_ui_response(&ok_value, &active).expect("parse input ok");
        assert_eq!(ok.value, Some(json!("hi")));

        let bad_value = json!({"type":"extension_ui_response","requestId":"req-1","value":123});
        assert!(
            rpc_parse_extension_ui_response(&bad_value, &active).is_err(),
            "non-string input should error"
        );
    }

    #[test]
    fn parse_editor_requires_string_value() {
        let active = ExtensionUiRequest::new("req-1", "editor", json!({"title":"t"}));
        let ok = json!({"requestId":"req-1","value":"multi\nline"});
        let resp = rpc_parse_extension_ui_response(&ok, &active).expect("editor ok");
        assert_eq!(resp.value, Some(json!("multi\nline")));

        let bad = json!({"requestId":"req-1","value":42});
        assert!(
            rpc_parse_extension_ui_response(&bad, &active).is_err(),
            "editor needs string"
        );
    }

    #[test]
    fn parse_notify_returns_no_value() {
        let active = ExtensionUiRequest::new("req-1", "notify", json!({"title":"t"}));
        let val = json!({"requestId":"req-1"});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("notify ok");
        assert!(!resp.cancelled);
        assert!(resp.value.is_none());
    }

    #[test]
    fn parse_unsupported_method_errors() {
        let active = ExtensionUiRequest::new("req-1", "custom_method", json!({}));
        let val = json!({"requestId":"req-1","value":"x"});
        let err = rpc_parse_extension_ui_response(&val, &active).unwrap_err();
        assert!(err.contains("Unsupported"), "err={err}");
    }

    #[test]
    fn parse_select_missing_value_field() {
        let active =
            ExtensionUiRequest::new("req-1", "select", json!({"title":"pick","options":["A"]}));
        let val = json!({"requestId":"req-1"});
        let err = rpc_parse_extension_ui_response(&val, &active).unwrap_err();
        assert!(err.contains("value"), "err={err}");
    }

    #[test]
    fn parse_confirm_missing_value_errors() {
        let active = ExtensionUiRequest::new("req-1", "confirm", json!({"title":"t"}));
        let val = json!({"requestId":"req-1"});
        let err = rpc_parse_extension_ui_response(&val, &active).unwrap_err();
        assert!(err.contains("confirm"), "err={err}");
    }

    #[test]
    fn parse_select_with_label_value_objects() {
        let active = ExtensionUiRequest::new(
            "req-1",
            "select",
            json!({
                "title": "pick",
                "options": [
                    {"label": "Alpha", "value": "a"},
                    {"label": "Beta", "value": "b"},
                ]
            }),
        );
        let val = json!({"requestId":"req-1","value":"a"});
        let resp = rpc_parse_extension_ui_response(&val, &active).expect("select by value");
        assert_eq!(resp.value, Some(json!("a")));
    }

    #[test]
    fn parse_id_rejects_empty_and_whitespace() {
        let val = json!({"requestId":"  ","id":""});
        assert!(rpc_parse_extension_ui_response_id(&val).is_none());
    }

    #[test]
    fn bridge_state_default_is_empty() {
        let state = RpcUiBridgeState::default();
        assert!(state.active.is_none());
        assert!(state.queue.is_empty());
    }
}

mod retry_tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentSession};
    use crate::model::{AssistantMessage, Usage};
    use crate::provider::Provider;
    use crate::resources::ResourceLoader;
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct FlakyProvider {
        calls: AtomicUsize,
    }

    impl FlakyProvider {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for FlakyProvider {
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
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);

            let mut partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };

            let events = if call == 0 {
                partial.stop_reason = StopReason::Error;
                partial.error_message = Some("server error".to_string());
                vec![
                    Ok(crate::model::StreamEvent::Start {
                        partial: partial.clone(),
                    }),
                    Ok(crate::model::StreamEvent::Error {
                        reason: StopReason::Error,
                        error: partial,
                    }),
                ]
            } else {
                vec![
                    Ok(crate::model::StreamEvent::Start {
                        partial: partial.clone(),
                    }),
                    Ok(crate::model::StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: partial,
                    }),
                ]
            };

            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[derive(Debug)]
    struct AlwaysErrorProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for AlwaysErrorProvider {
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
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            let mut partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("server error".to_string()),
                timestamp: 0,
            };

            let events = vec![
                Ok(crate::model::StreamEvent::Start {
                    partial: partial.clone(),
                }),
                Ok(crate::model::StreamEvent::Error {
                    reason: StopReason::Error,
                    error: {
                        partial.stop_reason = StopReason::Error;
                        partial
                    },
                }),
            ];

            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[test]
    fn rpc_auto_retry_retries_then_succeeds() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(FlakyProvider::new());
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let inner_session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let session = Arc::new(Mutex::new(agent_session));

            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(1),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(1),
                max_delay_ms: Some(1),
            });

            let mut shared = RpcSharedState::new(&config);
            shared.auto_compaction_enabled = false;
            let shared_state = Arc::new(Mutex::new(shared));

            let is_streaming = Arc::new(AtomicBool::new(false));
            let is_compacting = Arc::new(AtomicBool::new(false));
            let abort_handle_slot: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
            let retry_abort = Arc::new(AtomicBool::new(false));
            let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();

            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("auth load");

            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: Vec::new(),
                scoped_models: Vec::new(),
                auth,
                runtime_handle,
            };

            run_prompt_with_retry(
                session,
                shared_state,
                is_streaming,
                is_compacting,
                abort_handle_slot,
                out_tx,
                retry_abort,
                options,
                "hello".to_string(),
                Vec::new(),
                AgentCx::for_request(),
            )
            .await;

            let mut saw_retry_start = false;
            let mut saw_retry_end_success = false;

            for line in out_rx.try_iter() {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(kind) = value.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "auto_retry_start" => {
                        saw_retry_start = true;
                    }
                    "auto_retry_end" => {
                        if value.get("success").and_then(Value::as_bool) == Some(true) {
                            saw_retry_end_success = true;
                        }
                    }
                    _ => {}
                }
            }

            assert!(saw_retry_start, "missing auto_retry_start event");
            assert!(
                saw_retry_end_success,
                "missing successful auto_retry_end event"
            );
        });
    }

    #[test]
    fn rpc_abort_retry_emits_ordered_retry_timeline() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(AlwaysErrorProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let inner_session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session = AgentSession::new(
                agent,
                inner_session,
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );

            let session = Arc::new(Mutex::new(agent_session));

            let mut config = Config::default();
            config.retry = Some(crate::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(3),
                reviewer_model: None,
                token_budget: None,
                base_delay_ms: Some(100),
                max_delay_ms: Some(100),
            });

            let mut shared = RpcSharedState::new(&config);
            shared.auto_compaction_enabled = false;
            let shared_state = Arc::new(Mutex::new(shared));

            let is_streaming = Arc::new(AtomicBool::new(false));
            let is_compacting = Arc::new(AtomicBool::new(false));
            let abort_handle_slot: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));
            let retry_abort = Arc::new(AtomicBool::new(false));
            let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();

            let auth_path = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("auth load");

            let options = RpcOptions {
                config,
                resources: ResourceLoader::empty(false),
                available_models: Vec::new(),
                scoped_models: Vec::new(),
                auth,
                runtime_handle,
            };

            let retry_abort_for_thread = Arc::clone(&retry_abort);
            let abort_thread = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                retry_abort_for_thread.store(true, Ordering::SeqCst);
            });

            run_prompt_with_retry(
                session,
                shared_state,
                is_streaming,
                is_compacting,
                abort_handle_slot,
                out_tx,
                retry_abort,
                options,
                "hello".to_string(),
                Vec::new(),
                AgentCx::for_request(),
            )
            .await;
            abort_thread.join().expect("abort thread join");

            let mut timeline = Vec::new();
            let mut last_agent_end_error = None::<String>;

            for line in out_rx.try_iter() {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(kind) = value.get("type").and_then(Value::as_str) else {
                    continue;
                };
                timeline.push(kind.to_string());
                if kind == "agent_end" {
                    last_agent_end_error = value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }

            let retry_start_idx = timeline
                .iter()
                .position(|kind| kind == "auto_retry_start")
                .expect("missing auto_retry_start");
            let retry_end_idx = timeline
                .iter()
                .position(|kind| kind == "auto_retry_end")
                .expect("missing auto_retry_end");
            let agent_end_idx = timeline
                .iter()
                .rposition(|kind| kind == "agent_end")
                .expect("missing agent_end");

            assert!(
                retry_start_idx < retry_end_idx && retry_end_idx < agent_end_idx,
                "unexpected retry timeline ordering: {timeline:?}"
            );
            assert_eq!(
                last_agent_end_error.as_deref(),
                Some("Retry aborted"),
                "expected retry-abort terminal error, timeline: {timeline:?}"
            );
        });
    }
}

pub(super) fn parse_thinking_level(level: &str) -> Result<crate::model::ThinkingLevel> {
    level.parse().map_err(|err: String| Error::validation(err))
}
