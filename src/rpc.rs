//! RPC mode: headless JSON protocol over stdin/stdout.
//!
//! This implements a compatibility subset of pi-mono's RPC protocol
//! (see legacy `docs/rpc.md` in `legacy_pi_mono_code`).

#![allow(clippy::significant_drop_tightening)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::needless_pass_by_value)]

use crate::agent::AgentSession;
use crate::auth::AuthStorage;
use crate::config::{Config, ReliabilityEnforcementMode};
use crate::error::{Error, Result};
use crate::models::ModelEntry;
use crate::orchestration::RunStatus;
use crate::provider_metadata::canonical_provider_id;
use crate::reliability;
use crate::resources::ResourceLoader;
use asupersync::channel::mpsc;
use asupersync::runtime::RuntimeHandle;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[cfg(test)]
use crate::agent::{AbortHandle, AgentEvent};
#[cfg(test)]
use crate::agent_cx::AgentCx;
#[cfg(test)]
use crate::compaction::{
    ResolvedCompactionSettings, compact, compaction_details_to_value, prepare_compaction,
};
#[cfg(test)]
use crate::contracts::dto::SessionIdentity;
#[cfg(test)]
use crate::extensions::ExtensionManager;
#[cfg(test)]
use crate::model::{ContentBlock, ImageContent, StopReason, TextContent, UserContent, UserMessage};
#[cfg(test)]
use crate::orchestration::ExecutionTier;
#[cfg(test)]
use crate::orchestration::{
    RunLifecycle, RunStore, RunVerifyStatus, SubrunPlan, TaskReport, WaveStatus,
};
#[cfg(test)]
use crate::services::reliability_service::ReliabilityService;
#[cfg(test)]
use crate::services::run_service;
#[cfg(test)]
use crate::services::run_service::CompletedRunVerifyScope;
#[cfg(test)]
use crate::surface::rpc_protocol::{error_hints_value, normalize_command_type, response_error};
#[cfg(test)]
use crate::surface::rpc_server::try_send_line_with_backpressure;
#[cfg(test)]
use crate::surface::rpc_support::{
    RpcSharedState, RpcStateSnapshot, RpcUiBridgeState, RunningBash, apply_model_change,
    apply_thinking_level, available_thinking_levels, current_model_entry, cycle_model_for_rpc,
    export_html_snapshot, fork_messages_from_entries, last_assistant_text,
    model_requires_configured_credential, resolve_model_key, rpc_model_from_entry,
    rpc_session_message_value, session_state, session_stats, sync_agent_queue_modes,
};
#[cfg(test)]
use crate::surface::rpc_transport_commands::{
    StreamingBehavior, build_user_message, is_extension_command, line_count_from_newline_count,
    parse_prompt_images, parse_streaming_behavior, retry_delay_ms, rpc_parse_extension_ui_response,
    rpc_parse_extension_ui_response_id, run_bash_rpc, run_prompt_with_retry, should_auto_compact,
};
#[cfg(test)]
use asupersync::channel::oneshot;
#[cfg(test)]
use asupersync::sync::Mutex;
#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn provider_ids_match(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.eq_ignore_ascii_case(right) {
        return true;
    }

    let left_canonical = canonical_provider_id(left).unwrap_or(left);
    let right_canonical = canonical_provider_id(right).unwrap_or(right);

    left_canonical.eq_ignore_ascii_case(right)
        || right_canonical.eq_ignore_ascii_case(left)
        || left_canonical.eq_ignore_ascii_case(right_canonical)
}

#[derive(Clone)]
pub struct RpcOptions {
    pub config: Config,
    pub resources: ResourceLoader,
    pub available_models: Vec<ModelEntry>,
    pub scoped_models: Vec<RpcScopedModel>,
    pub auth: AuthStorage,
    pub runtime_handle: RuntimeHandle,
}

#[derive(Debug, Clone)]
pub struct RpcScopedModel {
    pub model: ModelEntry,
    pub thinking_level: Option<crate::model::ThinkingLevel>,
}

pub type EvidenceRecord = reliability::EvidenceRecord;
pub type ClosePayload = reliability::ClosePayload;
pub type StateDigest = reliability::StateDigest;
pub type ArtifactQuery = reliability::ArtifactQuery;

const fn default_prerequisite_trigger() -> reliability::EdgeTrigger {
    reliability::EdgeTrigger::OnSuccess
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPrerequisite {
    pub task_id: String,
    #[serde(default = "default_prerequisite_trigger")]
    pub trigger: reliability::EdgeTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContract {
    pub task_id: String,
    pub objective: String,
    #[serde(default)]
    pub parent_goal_trace_id: Option<String>,
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(default)]
    pub max_touched_files: Option<u16>,
    #[serde(default)]
    pub forbid_paths: Vec<String>,
    #[serde(default)]
    pub verify_command: String,
    #[serde(default)]
    pub verify_timeout_sec: Option<u32>,
    #[serde(default)]
    pub max_attempts: Option<u8>,
    #[serde(default)]
    pub input_snapshot: Option<String>,
    #[serde(default)]
    pub acceptance_ids: Vec<String>,
    #[serde(default)]
    pub planned_touches: Vec<String>,
    #[serde(default)]
    pub prerequisites: Vec<TaskPrerequisite>,
    #[serde(default)]
    pub enforce_symbol_drift_check: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchGrant {
    pub task_id: String,
    pub agent_id: String,
    pub lease_id: String,
    pub fence_token: u64,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockerReport {
    pub task_id: String,
    pub lease_id: String,
    pub fence_token: u64,
    pub reason: String,
    pub context: String,
    #[serde(default)]
    pub defer_trigger: Option<reliability::DeferTrigger>,
    #[serde(default)]
    pub resolved: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppendEvidenceRequest {
    pub task_id: String,
    pub command: String,
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub env_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubmitTaskRequest {
    pub task_id: String,
    pub lease_id: String,
    pub fence_token: u64,
    pub patch_digest: String,
    pub verify_run_id: String,
    #[serde(default)]
    pub verify_passed: Option<bool>,
    #[serde(default)]
    pub verify_timed_out: bool,
    #[serde(default)]
    pub failure_class: Option<reliability::FailureClass>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub symbol_drift_violations: Vec<String>,
    #[serde(default)]
    pub close: Option<ClosePayload>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubmitTaskResponse {
    pub(crate) task_id: String,
    pub(crate) state: String,
    pub(crate) close_payload: ClosePayload,
    pub(crate) close: reliability::CloseResult,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StateDigestRequest {
    #[serde(default)]
    pub(crate) task_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartRunRequest {
    #[serde(default)]
    pub(crate) run_id: Option<String>,
    pub(crate) objective: String,
    pub(crate) tasks: Vec<TaskContract>,
    pub(crate) run_verify_command: String,
    #[serde(default)]
    pub(crate) run_verify_timeout_sec: Option<u32>,
    #[serde(default)]
    pub(crate) max_parallelism: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunLookupRequest {
    pub(crate) run_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelRunRequest {
    pub(crate) run_id: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DispatchRunRequest {
    pub(crate) run_id: String,
    #[serde(default)]
    pub(crate) agent_id_prefix: Option<String>,
    #[serde(default)]
    pub(crate) lease_ttl_sec: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct RpcReliabilityState {
    pub(crate) enabled: bool,
    pub(crate) enforcement_mode: ReliabilityEnforcementMode,
    pub(crate) require_evidence_for_close: bool,
    pub(crate) default_max_attempts: u8,
    pub(crate) verify_timeout_sec_default: u32,
    pub(crate) max_touched_files: u16,
    pub(crate) allow_open_ended_defer: bool,
    pub(crate) tasks: HashMap<String, reliability::TaskNode>,
    pub(crate) evidence_by_task: HashMap<String, Vec<EvidenceRecord>>,
    pub(crate) latest_digest_by_task: HashMap<String, StateDigest>,
    pub(crate) edges: Vec<reliability::ReliabilityEdge>,
    pub(crate) symbol_drift_required_by_task: HashMap<String, bool>,
    pub(crate) parent_goal_trace_by_task: HashMap<String, String>,
    pub(crate) leases: reliability::LeaseManager,
    pub(crate) artifacts: reliability::FsArtifactStore,
}

#[derive(Debug, Default)]
pub(crate) struct RpcOrchestrationState {
    runs: HashMap<String, RunStatus>,
    task_runs: HashMap<String, HashSet<String>>,
}

impl RpcOrchestrationState {
    pub(crate) fn register_run(&mut self, run: RunStatus) {
        self.update_task_run_index(&run.run_id, &run.task_ids);
        self.runs.insert(run.run_id.clone(), run);
    }

    fn update_task_run_index(&mut self, run_id: &str, task_ids: &[String]) {
        for run_ids in self.task_runs.values_mut() {
            run_ids.remove(run_id);
        }
        for task_id in task_ids {
            self.task_runs
                .entry(task_id.clone())
                .or_default()
                .insert(run_id.to_string());
        }
        self.task_runs.retain(|_, run_ids| !run_ids.is_empty());
    }

    pub(crate) fn get_run(&self, run_id: &str) -> Option<RunStatus> {
        self.runs.get(run_id).cloned()
    }

    pub(crate) fn get_run_mut(&mut self, run_id: &str) -> Option<&mut RunStatus> {
        self.runs.get_mut(run_id)
    }

    pub(crate) fn update_run(&mut self, run: RunStatus) {
        self.update_task_run_index(&run.run_id, &run.task_ids);
        self.runs.insert(run.run_id.clone(), run);
    }

    pub(crate) fn run_ids_for_task(&self, task_id: &str) -> Vec<String> {
        self.task_runs
            .get(task_id)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn all_runs(&self) -> Vec<RunStatus> {
        self.runs.values().cloned().collect()
    }

    pub(crate) fn task_run_entries(&self) -> Vec<(String, Vec<String>)> {
        self.task_runs
            .iter()
            .map(|(task_id, run_ids)| {
                (task_id.clone(), run_ids.iter().cloned().collect::<Vec<_>>())
            })
            .collect()
    }
}

impl RpcReliabilityState {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Self::new_with_lease_provider(config, None)
    }

    pub(crate) fn new_with_lease_provider(
        config: &Config,
        external_provider: Option<Arc<dyn reliability::LeaseProvider>>,
    ) -> Result<Self> {
        let artifacts_root = Config::global_dir().join("reliability").join("artifacts");
        let artifacts = reliability::FsArtifactStore::new(artifacts_root)
            .map_err(|err| Error::session(format!("artifact store init failed: {err}")))?;
        let mut leases = reliability::LeaseManager::default();
        match config.reliability_lease_provider().unwrap_or("local") {
            "local" => {}
            "external" => {
                leases.set_external_provider(Arc::new(
                    reliability::InMemoryExternalLeaseProvider::default(),
                ));
            }
            other => {
                return Err(Error::validation(format!(
                    "unsupported reliability lease provider `{other}` (supported: local, external)"
                )));
            }
        }
        if let Some(provider) = external_provider {
            leases.set_external_provider(provider);
        }

        Ok(Self {
            enabled: config.reliability_enabled(),
            enforcement_mode: config.reliability_enforcement_mode(),
            require_evidence_for_close: config.reliability_require_evidence_for_close(),
            default_max_attempts: config.reliability_default_max_attempts(),
            verify_timeout_sec_default: config.reliability_verify_timeout_sec_default(),
            max_touched_files: config.reliability_max_touched_files(),
            allow_open_ended_defer: config.reliability_allow_open_ended_defer(),
            tasks: HashMap::new(),
            evidence_by_task: HashMap::new(),
            latest_digest_by_task: HashMap::new(),
            edges: Vec::new(),
            symbol_drift_required_by_task: HashMap::new(),
            parent_goal_trace_by_task: HashMap::new(),
            leases,
            artifacts,
        })
    }

    #[cfg(test)]
    fn set_external_lease_provider(&mut self, provider: Arc<dyn reliability::LeaseProvider>) {
        self.leases.set_external_provider(provider);
    }

    #[cfg(test)]
    const fn mode_blocks(&self) -> bool {
        ReliabilityService::mode_blocks(self)
    }

    #[cfg(test)]
    fn is_synthetic_trace_parent(trace_parent: &str) -> bool {
        ReliabilityService::is_synthetic_trace_parent(trace_parent)
    }

    #[cfg(test)]
    fn validate_close_trace_chain(
        &self,
        task_id: &str,
        close_payload: &ClosePayload,
    ) -> Result<()> {
        ReliabilityService::validate_close_trace_chain(self, task_id, close_payload)
    }

    #[cfg(test)]
    pub(crate) fn ensure_enabled(&self) -> Result<()> {
        if self.enabled {
            Ok(())
        } else {
            Err(Error::validation("Reliability is disabled in config"))
        }
    }

    #[cfg(test)]
    pub(crate) const fn state_label(state: &reliability::RuntimeState) -> &'static str {
        ReliabilityService::state_label(state)
    }

    #[cfg(test)]
    fn get_or_create_task(
        &mut self,
        contract: &TaskContract,
    ) -> Result<&mut reliability::TaskNode> {
        ReliabilityService::get_or_create_task(self, contract)
    }

    #[cfg(test)]
    fn task_counts_for(&self, task_ids: &[String]) -> BTreeMap<String, usize> {
        ReliabilityService::task_counts_for(self, task_ids)
    }

    #[cfg(test)]
    fn trigger_satisfied(
        trigger: &reliability::EdgeTrigger,
        terminal: &reliability::TerminalState,
    ) -> bool {
        ReliabilityService::trigger_satisfied(trigger, terminal)
    }

    #[cfg(test)]
    fn trigger_key(trigger: &reliability::EdgeTrigger) -> String {
        ReliabilityService::trigger_key(trigger)
    }

    #[cfg(test)]
    pub(crate) fn reconcile_prerequisites(&mut self, contract: &TaskContract) -> Result<()> {
        ReliabilityService::reconcile_prerequisites(self, contract)
    }

    #[cfg(test)]
    fn waiting_on_for_task(&self, task_id: &str) -> Vec<String> {
        ReliabilityService::waiting_on_for_task(self, task_id)
    }

    #[cfg(test)]
    pub(crate) fn project_waiting_on_for_task(&mut self, task_id: &str) -> Vec<String> {
        ReliabilityService::project_waiting_on_for_task(self, task_id)
    }

    #[cfg(test)]
    fn evaluate_dag_unblock(&mut self) -> reliability::DagEvaluation {
        ReliabilityService::evaluate_dag_unblock(self)
    }

    #[cfg(test)]
    fn promote_recoverable_due(&mut self) -> Vec<reliability::RecoveryAction> {
        ReliabilityService::promote_recoverable_due(self)
    }

    #[cfg(test)]
    pub(crate) fn refresh_dependency_states(&mut self) {
        ReliabilityService::refresh_dependency_states(self);
    }

    #[cfg(test)]
    fn request_dispatch(
        &mut self,
        contract: &TaskContract,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<DispatchGrant> {
        ReliabilityService::request_dispatch(self, contract, agent_id, ttl_seconds)
    }

    #[cfg(test)]
    pub(crate) fn request_dispatch_existing(
        &mut self,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<DispatchGrant> {
        ReliabilityService::request_dispatch_existing_state(self, task_id, agent_id, ttl_seconds)
    }

    #[cfg(test)]
    pub(crate) fn expire_dispatch_grant(&mut self, grant: &DispatchGrant) -> Result<()> {
        ReliabilityService::expire_dispatch_grant(self, grant)
    }

    #[cfg(test)]
    fn append_evidence(&mut self, req: AppendEvidenceRequest) -> Result<EvidenceRecord> {
        ReliabilityService::append_evidence_state(self, req)
    }

    #[cfg(test)]
    fn submit_task(&mut self, req: SubmitTaskRequest) -> Result<SubmitTaskResponse> {
        ReliabilityService::submit_task_state(self, req)
    }

    #[cfg(test)]
    fn resolve_blocker(&mut self, report: BlockerReport) -> Result<String> {
        ReliabilityService::resolve_blocker(self, report)
    }

    #[cfg(test)]
    fn query_artifact(&self, query: ArtifactQuery) -> Result<Vec<String>> {
        ReliabilityService::query_artifact(self, query)
    }

    #[cfg(test)]
    fn load_artifact_text(&self, artifact_id: &str) -> Result<String> {
        ReliabilityService::load_artifact_text(self, artifact_id)
    }

    #[cfg(test)]
    pub(crate) fn first_task_id(&self) -> Option<String> {
        self.tasks.keys().next().cloned()
    }

    #[cfg(test)]
    fn get_state_digest(&mut self, task_id: &str) -> Result<StateDigest> {
        ReliabilityService::get_state_digest_state(self, task_id)
    }
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
use test_support::*;

pub async fn run_stdio(session: AgentSession, options: RpcOptions) -> Result<()> {
    crate::surface::rpc_server::run_stdio(session, options).await
}

pub async fn run(
    session: AgentSession,
    options: RpcOptions,
    in_rx: mpsc::Receiver<String>,
    out_tx: std::sync::mpsc::Sender<String>,
) -> Result<()> {
    crate::surface::rpc_server::run(session, options, in_rx, out_tx).await
}

#[cfg(test)]
mod tests;
