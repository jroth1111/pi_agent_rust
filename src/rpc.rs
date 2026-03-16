//! RPC mode: headless JSON protocol over stdin/stdout.
//!
//! This exposes the project's headless JSON orchestration protocol.

#![allow(clippy::significant_drop_tightening)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::needless_pass_by_value)]

use crate::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession, QueueMode};
use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::compaction::{
    ResolvedCompactionSettings, compact, compaction_details_to_value, prepare_compaction,
};
use crate::config::{Config, ReliabilityEnforcementMode};
use crate::error::{Error, Result};
use crate::error_hints;
use crate::extensions::{ExtensionManager, ExtensionUiRequest, ExtensionUiResponse};
use crate::model::{
    AssistantMessage, ContentBlock, ImageContent, Message, StopReason, TextContent, UserContent,
    UserMessage,
};
use crate::models::ModelEntry;
#[cfg(test)]
use crate::orchestration::WaveStatus;
use crate::orchestration::{
    ExecutionTier, FlockWorkspace, RunVerifyScopeKind, RunVerifyStatus, TaskReport,
};
use crate::provider::Provider;
use crate::provider_metadata::{
    canonical_provider_id, provider_metadata, provider_routing_defaults,
};
use crate::providers;
use crate::reliability;
use crate::reliability::ArtifactStore;
use crate::reliability::verifier::Verifier;
use crate::resources::ResourceLoader;
use crate::runtime::controller::{
    ControllerOutput as RuntimeControllerOutput, RuntimeCommand, RuntimeController,
};
use crate::runtime::model_routing::{ModelRoute, PhaseModelRouter};
use crate::runtime::policy::PolicySet;
use crate::runtime::scheduler;
use crate::runtime::store::RuntimeStore;
use crate::runtime::types::{
    AutonomyLevel, LeaseRecord, ModelProfile, ModelSelector, PlanArtifact, RunBudgets,
    RunConstraints, RunPhase, RunSnapshot, RunSpec, TaskConstraints, TaskNode, TaskSpec, TaskState,
    VerifySpec,
};
use crate::session::{Session, SessionMessage};
use crate::tools::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};
use asupersync::channel::{mpsc, oneshot};
use asupersync::runtime::RuntimeHandle;
use asupersync::sync::{Mutex, OwnedMutexGuard};
use asupersync::time::{sleep, wall_now};
use async_trait::async_trait;
use chrono::Utc;
use memchr::memchr_iter;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn provider_ids_match(left: &str, right: &str) -> bool {
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
struct AppendEvidenceRequest {
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
struct SubmitTaskRequest {
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
    pub close: Option<reliability::ClosePayload>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitTaskResponse {
    task_id: String,
    state: String,
    close_payload: reliability::ClosePayload,
    close: reliability::CloseResult,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRunRequest {
    #[serde(default)]
    run_id: Option<String>,
    objective: String,
    tasks: Vec<TaskContract>,
    run_verify_command: String,
    #[serde(default)]
    run_verify_timeout_sec: Option<u32>,
    #[serde(default)]
    max_parallelism: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunLookupRequest {
    run_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelRunRequest {
    run_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DispatchRunRequest {
    run_id: String,
    #[serde(default)]
    agent_id_prefix: Option<String>,
    #[serde(default)]
    lease_ttl_sec: Option<i64>,
}

#[derive(Debug)]
struct RpcReliabilityState {
    enabled: bool,
    enforcement_mode: ReliabilityEnforcementMode,
    require_evidence_for_close: bool,
    default_max_attempts: u8,
    verify_timeout_sec_default: u32,
    max_touched_files: u16,
    allow_open_ended_defer: bool,
    tasks: HashMap<String, reliability::TaskNode>,
    evidence_by_task: HashMap<String, Vec<reliability::EvidenceRecord>>,
    latest_digest_by_task: HashMap<String, reliability::StateDigest>,
    edges: Vec<reliability::ReliabilityEdge>,
    symbol_drift_required_by_task: HashMap<String, bool>,
    parent_goal_trace_by_task: HashMap<String, String>,
    leases: reliability::LeaseManager,
    artifacts: reliability::FsArtifactStore,
}

impl RpcReliabilityState {
    fn new(config: &Config) -> Result<Self> {
        Self::new_with_lease_provider(config, None)
    }

    fn new_with_lease_provider(
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

    const fn mode_blocks(&self) -> bool {
        matches!(self.enforcement_mode, ReliabilityEnforcementMode::Hard)
    }

    fn is_synthetic_trace_parent(trace_parent: &str) -> bool {
        let normalized = trace_parent.trim().to_ascii_lowercase();
        matches!(
            normalized.as_str(),
            "" | "rpc:auto"
                | "auto"
                | "default"
                | "unknown"
                | "none"
                | "n/a"
                | "na"
                | "todo"
                | "tbd"
        )
    }

    fn validate_close_trace_chain(
        &self,
        task_id: &str,
        close_payload: &reliability::ClosePayload,
    ) -> Result<()> {
        let trace_parent = close_payload
            .trace_parent
            .as_ref()
            .map(|trace| trace.trim())
            .filter(|trace| !trace.is_empty())
            .ok_or_else(|| {
                Error::validation("close payload missing parent-goal trace reference")
            })?;

        if Self::is_synthetic_trace_parent(trace_parent) {
            return Err(Error::validation(format!(
                "close trace_parent `{trace_parent}` is synthetic; provide real parent-goal trace ID"
            )));
        }

        let expected = self
            .parent_goal_trace_by_task
            .get(task_id)
            .map(|trace| trace.trim())
            .filter(|trace| !trace.is_empty())
            .ok_or_else(|| {
                Error::validation(format!(
                    "task `{task_id}` missing parent_goal_trace_id in dispatch contract"
                ))
            })?;

        if expected != trace_parent {
            return Err(Error::validation(format!(
                "trace chain mismatch: task `{task_id}` expects parent-goal trace `{expected}`, got `{trace_parent}`"
            )));
        }

        Ok(())
    }

    fn ensure_enabled(&self) -> Result<()> {
        if self.enabled {
            Ok(())
        } else {
            Err(Error::validation("Reliability is disabled in config"))
        }
    }

    const fn state_label(state: &reliability::RuntimeState) -> &'static str {
        match state {
            reliability::RuntimeState::Blocked { .. } => "blocked",
            reliability::RuntimeState::Ready => "ready",
            reliability::RuntimeState::Leased { .. } => "leased",
            reliability::RuntimeState::Verifying { .. } => "verifying",
            reliability::RuntimeState::Recoverable { .. } => "recoverable",
            reliability::RuntimeState::AwaitingHuman { .. } => "awaiting_human",
            reliability::RuntimeState::Terminal(_) => "terminal",
        }
    }

    fn get_or_create_task(
        &mut self,
        contract: &TaskContract,
    ) -> Result<&mut reliability::TaskNode> {
        let verify_command = if contract.verify_command.trim().is_empty() {
            "cargo test".to_string()
        } else {
            contract.verify_command.clone()
        };
        let spec = reliability::TaskSpec {
            objective: contract.objective.clone(),
            constraints: reliability::TaskConstraintSet {
                invariants: contract.invariants.clone(),
                max_touched_files: contract.max_touched_files.or(Some(self.max_touched_files)),
                forbid_paths: contract.forbid_paths.clone(),
                network_access: reliability::NetworkPolicy::Offline,
            },
            verify: reliability::VerifyPlan::Standard {
                audit_diff: true,
                command: verify_command,
                timeout_sec: contract
                    .verify_timeout_sec
                    .unwrap_or(self.verify_timeout_sec_default),
            },
            max_attempts: contract.max_attempts.unwrap_or(self.default_max_attempts),
            input_snapshot: contract
                .input_snapshot
                .clone()
                .unwrap_or_else(|| "rpc-dispatch".to_string()),
            acceptance_ids: contract.acceptance_ids.clone(),
            planned_touches: contract.planned_touches.clone(),
        };
        spec.validate()
            .map_err(|err| Error::validation(err.to_string()))?;

        let entry = self
            .tasks
            .entry(contract.task_id.clone())
            .or_insert_with(|| reliability::TaskNode::new(contract.task_id.clone(), spec.clone()));
        entry.spec = spec;
        self.symbol_drift_required_by_task.insert(
            contract.task_id.clone(),
            contract.enforce_symbol_drift_check,
        );
        if let Some(parent_trace) = contract
            .parent_goal_trace_id
            .as_ref()
            .map(|trace| trace.trim())
            .filter(|trace| !trace.is_empty())
        {
            self.parent_goal_trace_by_task
                .insert(contract.task_id.clone(), parent_trace.to_string());
        } else {
            self.parent_goal_trace_by_task.remove(&contract.task_id);
        }
        Ok(entry)
    }

    fn trigger_satisfied(
        trigger: &reliability::EdgeTrigger,
        terminal: &reliability::TerminalState,
    ) -> bool {
        match trigger {
            reliability::EdgeTrigger::Always => true,
            reliability::EdgeTrigger::OnSuccess => {
                matches!(terminal, reliability::TerminalState::Succeeded { .. })
            }
            reliability::EdgeTrigger::OnFailure(mask) => match terminal {
                reliability::TerminalState::Failed { class, .. } => mask.matches(*class),
                _ => false,
            },
        }
    }

    fn trigger_key(trigger: &reliability::EdgeTrigger) -> String {
        match trigger {
            reliability::EdgeTrigger::OnSuccess => "on_success".to_string(),
            reliability::EdgeTrigger::Always => "always".to_string(),
            reliability::EdgeTrigger::OnFailure(mask) => {
                let mut classes: Vec<String> = mask
                    .classes
                    .iter()
                    .map(|class| format!("{class:?}"))
                    .collect();
                classes.sort();
                format!("on_failure:{}", classes.join("|"))
            }
        }
    }

    fn reconcile_prerequisites(&mut self, contract: &TaskContract) -> Result<()> {
        let previous_edges = self.edges.clone();
        self.edges.retain(|edge| {
            !(edge.to == contract.task_id
                && matches!(edge.kind, reliability::EdgeKind::Prerequisite { .. }))
        });

        let mut dedupe = HashSet::new();
        for prerequisite in &contract.prerequisites {
            let from = prerequisite.task_id.trim();
            if from.is_empty() {
                continue;
            }
            let key = format!(
                "{}->{}:{}",
                from,
                contract.task_id,
                Self::trigger_key(&prerequisite.trigger)
            );
            if !dedupe.insert(key.clone()) {
                continue;
            }
            self.edges.push(reliability::ReliabilityEdge {
                id: format!("rpc_edge:{key}"),
                from: from.to_string(),
                to: contract.task_id.clone(),
                kind: reliability::EdgeKind::Prerequisite {
                    trigger: prerequisite.trigger.clone(),
                },
            });
        }
        self.edges.sort_by(|left, right| left.id.cmp(&right.id));

        if reliability::DagEvaluator::detect_cycle(&self.edges) {
            self.edges = previous_edges;
            return Err(Error::validation(format!(
                "reliability DAG cycle detected while integrating prerequisites for {}",
                contract.task_id
            )));
        }

        Ok(())
    }

    fn waiting_on_for_task(&self, task_id: &str) -> Vec<String> {
        let terminal_states: HashMap<String, reliability::TerminalState> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| match &task.runtime.state {
                reliability::RuntimeState::Terminal(terminal) => {
                    Some((id.clone(), terminal.clone()))
                }
                _ => None,
            })
            .collect();
        let mut waiting_on = Vec::new();
        for edge in &self.edges {
            if edge.to != task_id {
                continue;
            }
            let reliability::EdgeKind::Prerequisite { trigger } = &edge.kind else {
                continue;
            };
            match terminal_states.get(&edge.from) {
                Some(terminal) if Self::trigger_satisfied(trigger, terminal) => {}
                _ => waiting_on.push(edge.from.clone()),
            }
        }
        waiting_on.sort();
        waiting_on.dedup();
        waiting_on
    }

    fn project_waiting_on_for_task(&mut self, task_id: &str) -> Vec<String> {
        let waiting_on = self.waiting_on_for_task(task_id);
        let Some(task) = self.tasks.get_mut(task_id) else {
            return waiting_on;
        };

        match (&task.runtime.state, waiting_on.is_empty()) {
            (
                reliability::RuntimeState::Ready | reliability::RuntimeState::Blocked { .. },
                false,
            ) => {
                task.runtime.state = reliability::RuntimeState::Blocked {
                    waiting_on: waiting_on.clone(),
                };
            }
            (reliability::RuntimeState::Blocked { .. }, true) => {
                if reliability::apply_transition(
                    &mut task.runtime,
                    &reliability::TransitionEvent::DependenciesMet,
                    task.spec.max_attempts,
                )
                .is_err()
                {
                    task.runtime.state = reliability::RuntimeState::Ready;
                }
            }
            _ => {}
        }

        waiting_on
    }

    fn evaluate_dag_unblock(&mut self) -> reliability::DagEvaluation {
        let mut ordered_ids = self.tasks.keys().cloned().collect::<Vec<_>>();
        ordered_ids.sort();
        let mut task_snapshot = ordered_ids
            .iter()
            .filter_map(|task_id| self.tasks.get(task_id).cloned())
            .collect::<Vec<_>>();

        let evaluation =
            reliability::DagEvaluator::evaluate_and_unblock(&mut task_snapshot, &self.edges);
        for node in task_snapshot {
            if let Some(task) = self.tasks.get_mut(&node.id) {
                task.runtime = node.runtime;
            }
        }
        evaluation
    }

    fn promote_recoverable_due(&mut self) -> Vec<reliability::RecoveryAction> {
        let mut ordered_ids = self.tasks.keys().cloned().collect::<Vec<_>>();
        ordered_ids.sort();
        let mut task_snapshot = ordered_ids
            .iter()
            .filter_map(|task_id| self.tasks.get(task_id).cloned())
            .collect::<Vec<_>>();

        let promoted = reliability::RecoveryManager::promote_recoverable(&mut task_snapshot);
        for node in task_snapshot {
            if let Some(task) = self.tasks.get_mut(&node.id) {
                task.runtime = node.runtime;
            }
        }
        promoted
    }

    fn refresh_dependency_states(&mut self) {
        self.promote_recoverable_due();
        self.evaluate_dag_unblock();

        let mut ordered_ids = self.tasks.keys().cloned().collect::<Vec<_>>();
        ordered_ids.sort();
        for task_id in ordered_ids {
            self.project_waiting_on_for_task(&task_id);
        }
    }

    fn request_dispatch(
        &mut self,
        contract: &TaskContract,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<DispatchGrant> {
        self.ensure_enabled()?;
        let task_id = contract.task_id.clone();
        self.get_or_create_task(contract)?;
        self.reconcile_prerequisites(contract)?;
        self.refresh_dependency_states();
        self.request_dispatch_existing(&task_id, agent_id, ttl_seconds)
    }

    fn request_dispatch_existing(
        &mut self,
        task_id: &str,
        agent_id: &str,
        ttl_seconds: i64,
    ) -> Result<DispatchGrant> {
        self.ensure_enabled()?;
        let waiting_on = self.project_waiting_on_for_task(task_id);
        if !waiting_on.is_empty() {
            return Err(Error::validation(format!(
                "task {task_id} is blocked by prerequisites: {}",
                waiting_on.join(", ")
            )));
        }
        if let Some(task) = self.tasks.get(task_id) {
            if let reliability::RuntimeState::Recoverable { retry_after, .. } = &task.runtime.state
            {
                let retry_note = retry_after
                    .map(|ts| format!("retry after {}", ts.to_rfc3339()))
                    .unwrap_or_else(|| "retry window not yet promoted".to_string());
                return Err(Error::validation(format!(
                    "task {task_id} is recoverable and not ready to dispatch ({retry_note})"
                )));
            }
        }

        let grant = self
            .leases
            .issue_lease(task_id, agent_id, ttl_seconds)
            .map_err(|err| Error::validation(err.to_string()))?;

        let event = reliability::TransitionEvent::Dispatch {
            lease_id: grant.lease_id.clone(),
            agent_id: grant.agent_id.clone(),
            fence_token: grant.fence_token,
            expires_at: grant.expires_at,
        };

        let mode_blocks = self.mode_blocks();
        let (task_id, state) = {
            let Some(task) = self.tasks.get_mut(task_id) else {
                return Err(Error::validation("task disappeared during dispatch"));
            };

            if let Err(err) =
                reliability::apply_transition(&mut task.runtime, &event, task.spec.max_attempts)
            {
                let _ = self.leases.expire_lease(&grant.lease_id);
                if mode_blocks {
                    return Err(Error::validation(format!(
                        "dispatch transition rejected: {err}"
                    )));
                }
                task.runtime.state = reliability::RuntimeState::Ready;
                reliability::apply_transition(&mut task.runtime, &event, task.spec.max_attempts)
                    .map_err(|inner| {
                        Error::validation(format!("dispatch transition recovery failed: {inner}"))
                    })?;
            }
            (
                task.id.clone(),
                Self::state_label(&task.runtime.state).to_string(),
            )
        };

        Ok(DispatchGrant {
            task_id,
            agent_id: grant.agent_id,
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            expires_at: grant.expires_at,
            state,
        })
    }

    fn expire_dispatch_grant(&mut self, grant: &DispatchGrant) -> Result<()> {
        self.ensure_enabled()?;
        let _ = self.leases.expire_lease(&grant.lease_id);
        let Some(task) = self.tasks.get_mut(&grant.task_id) else {
            return Ok(());
        };

        if matches!(task.runtime.state, reliability::RuntimeState::Leased { .. }) {
            reliability::apply_transition(
                &mut task.runtime,
                &reliability::TransitionEvent::ExpireLease,
                task.spec.max_attempts,
            )
            .map_err(|err| Error::validation(format!("dispatch rollback failed: {err}")))?;
        }
        Ok(())
    }

    fn append_evidence(
        &mut self,
        req: AppendEvidenceRequest,
    ) -> Result<reliability::EvidenceRecord> {
        self.ensure_enabled()?;

        let mut artifact_ids = req.artifact_ids;
        if !req.stdout.is_empty() {
            let id = self
                .artifacts
                .put_text(&req.task_id, "stdout", &req.stdout)
                .map_err(|err| Error::session(format!("store stdout artifact failed: {err}")))?;
            artifact_ids.push(id);
        }
        if !req.stderr.is_empty() {
            let id = self
                .artifacts
                .put_text(&req.task_id, "stderr", &req.stderr)
                .map_err(|err| Error::session(format!("store stderr artifact failed: {err}")))?;
            artifact_ids.push(id);
        }

        let evidence = reliability::EvidenceRecord::from_command_output_with_env(
            req.task_id.clone(),
            req.command,
            req.exit_code,
            &req.stdout,
            &req.stderr,
            artifact_ids,
            req.env_id,
        );
        self.evidence_by_task
            .entry(req.task_id)
            .or_default()
            .push(evidence.clone());
        Ok(evidence)
    }

    fn submit_task(&mut self, req: SubmitTaskRequest) -> Result<SubmitTaskResponse> {
        self.ensure_enabled()?;
        let SubmitTaskRequest {
            task_id,
            lease_id,
            fence_token,
            patch_digest,
            verify_run_id,
            verify_passed,
            verify_timed_out,
            failure_class,
            changed_files,
            symbol_drift_violations,
            close,
        } = req;

        let evidence = self
            .evidence_by_task
            .get(&task_id)
            .cloned()
            .unwrap_or_default();
        let has_pass = evidence.iter().any(reliability::EvidenceRecord::is_success);
        let verify_passed = verify_passed.unwrap_or(has_pass);
        let mode_blocks = self.mode_blocks();
        let lease_id_for_release = lease_id.clone();

        if verify_passed && self.require_evidence_for_close {
            let verify_evidence = reliability::Verifier::ensure_evidence(&evidence, 1)
                .map_err(|err| Error::validation(err.to_string()))?;
            if mode_blocks && !verify_evidence.ok {
                return Err(Error::validation(verify_evidence.violations.join("; ")));
            }
        }

        let mut verifier_policy_violations = Vec::new();
        {
            let Some(task) = self.tasks.get(&task_id) else {
                return Err(Error::validation(format!(
                    "Unknown reliability task: {task_id}"
                )));
            };

            let scope_result = reliability::Verifier::audit_scope_with_constraints(
                &changed_files,
                &task.spec.constraints,
            )
            .map_err(|err| Error::validation(err.to_string()))?;
            if !scope_result.ok {
                verifier_policy_violations.extend(scope_result.violations);
            }

            if verify_timed_out {
                verifier_policy_violations.push(format!(
                    "verification timed out (configured timeout={}s)",
                    match &task.spec.verify {
                        reliability::VerifyPlan::Standard { timeout_sec, .. } => timeout_sec,
                    }
                ));
            }

            if self
                .symbol_drift_required_by_task
                .get(&task_id)
                .copied()
                .unwrap_or(false)
                && !symbol_drift_violations.is_empty()
            {
                verifier_policy_violations.push(format!(
                    "symbol/API drift detected: {}",
                    symbol_drift_violations.join("; ")
                ));
            }
        }
        if mode_blocks && !verifier_policy_violations.is_empty() {
            return Err(Error::validation(verifier_policy_violations.join("; ")));
        }

        let mut state = {
            let Some(task) = self.tasks.get_mut(&task_id) else {
                return Err(Error::validation(format!(
                    "Unknown reliability task: {task_id}"
                )));
            };
            reliability::apply_transition(
                &mut task.runtime,
                &reliability::TransitionEvent::Submit {
                    lease_id,
                    fence_token,
                    patch_digest: patch_digest.clone(),
                    verify_run_id: verify_run_id.clone(),
                },
                task.spec.max_attempts,
            )
            .map_err(|err| Error::validation(format!("submit transition rejected: {err}")))?;

            if verify_passed {
                reliability::apply_transition(
                    &mut task.runtime,
                    &reliability::TransitionEvent::VerifySuccess {
                        verify_run_id,
                        patch_digest,
                    },
                    task.spec.max_attempts,
                )
                .map_err(|err| {
                    Error::validation(format!("verify success transition rejected: {err}"))
                })?;
            } else {
                reliability::apply_transition(
                    &mut task.runtime,
                    &reliability::TransitionEvent::VerifyFail {
                        class: failure_class
                            .unwrap_or(reliability::FailureClass::VerificationFailed),
                        verify_run_id: Some(verify_run_id),
                        failure_artifact: None,
                        handoff_summary: "Verification failed during RPC submit_task".to_string(),
                    },
                    task.spec.max_attempts,
                )
                .map_err(|err| {
                    Error::validation(format!("verify fail transition rejected: {err}"))
                })?;
            }

            if !self.allow_open_ended_defer
                && matches!(
                    task.runtime.state,
                    reliability::RuntimeState::Recoverable { .. }
                )
            {
                reliability::RecoveryManager::set_retry_after(task, 60);
            }

            Self::state_label(&task.runtime.state).to_string()
        };
        let _ = self.leases.expire_lease(&lease_id_for_release);
        self.refresh_dependency_states();
        if let Some(task) = self.tasks.get(&task_id) {
            state = Self::state_label(&task.runtime.state).to_string();
        }

        let strict_close_trace_chain = mode_blocks && (verify_passed || close.is_some());
        let mut close_payload = close.unwrap_or_else(|| reliability::ClosePayload {
            task_id: task_id.clone(),
            outcome: "submitted via rpc".to_string(),
            outcome_kind: Some(reliability::CloseOutcomeKind::Success),
            acceptance_ids: if strict_close_trace_chain {
                Vec::new()
            } else {
                vec!["rpc:auto".to_string()]
            },
            evidence_ids: Vec::new(),
            trace_parent: if strict_close_trace_chain {
                None
            } else {
                Some("rpc:auto".to_string())
            },
        });
        if close_payload.task_id.trim().is_empty() {
            close_payload.task_id.clone_from(&task_id);
        }
        if close_payload
            .trace_parent
            .as_ref()
            .is_none_or(|trace| trace.trim().is_empty())
            && !strict_close_trace_chain
        {
            close_payload.trace_parent = Some("rpc:auto".to_string());
        }
        if close_payload.acceptance_ids.is_empty() && !strict_close_trace_chain {
            close_payload.acceptance_ids = vec!["rpc:auto".to_string()];
        }
        if close_payload.evidence_ids.is_empty() {
            close_payload.evidence_ids =
                evidence.iter().map(|rec| rec.evidence_id.clone()).collect();
        }
        if strict_close_trace_chain {
            self.validate_close_trace_chain(&task_id, &close_payload)?;
        }

        let mapping = reliability::Verifier::ensure_acceptance_mapped(
            &close_payload.acceptance_ids,
            &close_payload.evidence_ids,
        );
        if mode_blocks && !mapping.ok {
            return Err(Error::validation(mapping.violations.join("; ")));
        }

        let mut close = reliability::CloseResult::evaluate(
            &close_payload,
            &evidence,
            self.require_evidence_for_close,
        )
        .map_err(|err| Error::validation(err.to_string()))?;
        if !mapping.ok {
            close.approved = false;
            close.violations.extend(mapping.violations);
        }
        if !verifier_policy_violations.is_empty() {
            close.approved = false;
            close.violations.extend(verifier_policy_violations);
        }

        if mode_blocks {
            close
                .ensure_approved()
                .map_err(|err| Error::validation(err.to_string()))?;
        }

        Ok(SubmitTaskResponse {
            task_id,
            state,
            close_payload,
            close,
        })
    }

    fn resolve_blocker(&mut self, report: BlockerReport) -> Result<String> {
        self.ensure_enabled()?;
        let is_defer = report.reason.trim().eq_ignore_ascii_case("defer");
        let report_reason = report.reason.clone();
        let report_context = report.context.clone();
        let lease_id_for_release = report.lease_id.clone();
        if !report.resolved
            && is_defer
            && !self.allow_open_ended_defer
            && report.defer_trigger.is_none()
        {
            return Err(Error::validation(
                "Open-ended defer blocker is disabled by reliability policy; provide defer_trigger Until or DependsOn",
            ));
        }

        {
            let Some(task) = self.tasks.get_mut(&report.task_id) else {
                return Err(Error::validation(format!(
                    "Unknown reliability task: {}",
                    report.task_id
                )));
            };

            let event = if report.resolved {
                reliability::TransitionEvent::HumanResolve
            } else {
                reliability::TransitionEvent::ReportBlocker {
                    lease_id: report.lease_id,
                    fence_token: report.fence_token,
                    reason: report_reason,
                    context: report_context.clone(),
                }
            };
            reliability::apply_transition(&mut task.runtime, &event, task.spec.max_attempts)
                .map_err(|err| {
                    Error::validation(format!("resolve blocker transition rejected: {err}"))
                })?;

            if !report.resolved && is_defer {
                if let Some(trigger) = &report.defer_trigger {
                    match trigger {
                        reliability::DeferTrigger::Until { until } => {
                            task.runtime.state = reliability::RuntimeState::Recoverable {
                                reason: reliability::FailureClass::HumanBlocker,
                                failure_artifact: None,
                                handoff_summary: report_context,
                                retry_after: Some(*until),
                            };
                        }
                        reliability::DeferTrigger::DependsOn { task_id } => {
                            task.runtime.state = reliability::RuntimeState::Blocked {
                                waiting_on: vec![task_id.clone()],
                            };
                        }
                    }
                }
            }
        }

        if !report.resolved {
            let _ = self.leases.expire_lease(&lease_id_for_release);
        }

        self.refresh_dependency_states();
        let Some(task) = self.tasks.get(&report.task_id) else {
            return Err(Error::validation(format!(
                "Unknown reliability task: {}",
                report.task_id
            )));
        };
        Ok(Self::state_label(&task.runtime.state).to_string())
    }

    fn query_artifact(&self, query: reliability::ArtifactQuery) -> Result<Vec<String>> {
        self.ensure_enabled()?;
        self.artifacts
            .list(&query)
            .map_err(|err| Error::session(format!("artifact query failed: {err}")))
    }

    fn load_artifact_text(&self, artifact_id: &str) -> Result<String> {
        self.ensure_enabled()?;
        let bytes = self
            .artifacts
            .load(artifact_id)
            .map_err(|err| Error::session(format!("artifact load failed: {err}")))?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    fn first_task_id(&self) -> Option<String> {
        self.tasks.keys().next().cloned()
    }

    fn get_state_digest(&mut self, task_id: &str) -> Result<reliability::StateDigest> {
        self.ensure_enabled()?;
        self.refresh_dependency_states();
        let Some(task) = self.tasks.get(task_id) else {
            return Err(Error::validation(format!(
                "Unknown reliability task: {task_id}"
            )));
        };

        let mut digest = reliability::StateDigest::new(
            task.spec.objective.clone(),
            Self::state_label(&task.runtime.state),
        );
        digest
            .recent_actions
            .push(format!("attempt={}", task.runtime.attempt));
        digest
            .recent_actions
            .push(format!("max_attempts={}", task.spec.max_attempts));

        match &task.runtime.state {
            reliability::RuntimeState::Blocked { waiting_on } => {
                digest.blockers.clone_from(waiting_on);
                digest.next_action = Some("Resolve blockers".to_string());
            }
            reliability::RuntimeState::AwaitingHuman { question, .. } => {
                digest.blockers.push(question.clone());
                digest.next_action = Some("Resolve human blocker".to_string());
            }
            reliability::RuntimeState::Recoverable { retry_after, .. } => {
                digest.next_action = Some(retry_after.as_ref().map_or_else(
                    || "Promote recoverable task".to_string(),
                    |ts| format!("Retry after {}", ts.to_rfc3339()),
                ));
            }
            reliability::RuntimeState::Ready => {
                digest.next_action = Some("Request dispatch".to_string());
            }
            reliability::RuntimeState::Leased { .. } => {
                digest.next_action = Some("Submit task for verification".to_string());
            }
            reliability::RuntimeState::Verifying { .. } => {
                digest.next_action = Some("Finalize verify result".to_string());
            }
            reliability::RuntimeState::Terminal(term) => {
                let terminal = match term {
                    reliability::TerminalState::Succeeded { .. } => "succeeded",
                    reliability::TerminalState::Failed { .. } => "failed",
                    reliability::TerminalState::Superseded { .. } => "superseded",
                    reliability::TerminalState::Canceled { .. } => "canceled",
                };
                digest.next_action = Some(format!("Terminal: {terminal}"));
            }
        }

        self.latest_digest_by_task
            .insert(task_id.to_string(), digest.clone());
        Ok(digest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone)]
struct RpcStateSnapshot {
    steering_count: usize,
    follow_up_count: usize,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    auto_compaction_enabled: bool,
    auto_retry_enabled: bool,
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
    const fn pending_count(&self) -> usize {
        self.steering_count + self.follow_up_count
    }
}

use crate::config::parse_queue_mode;

fn parse_streaming_behavior(value: Option<&Value>) -> Result<Option<StreamingBehavior>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(s) = value.as_str() else {
        return Err(Error::validation("streamingBehavior must be a string"));
    };
    match s {
        "steer" => Ok(Some(StreamingBehavior::Steer)),
        "follow-up" | "followUp" => Ok(Some(StreamingBehavior::FollowUp)),
        _ => Err(Error::validation(format!("Invalid streamingBehavior: {s}"))),
    }
}

fn normalize_command_type(command_type: &str) -> &str {
    match command_type {
        "follow-up" | "followUp" | "queue-follow-up" | "queueFollowUp" => "follow_up",
        "get-state" | "getState" => "get_state",
        "set-model" | "setModel" => "set_model",
        "set-steering-mode" | "setSteeringMode" => "set_steering_mode",
        "set-follow-up-mode" | "setFollowUpMode" => "set_follow_up_mode",
        "set-auto-compaction" | "setAutoCompaction" => "set_auto_compaction",
        "set-auto-retry" | "setAutoRetry" => "set_auto_retry",
        _ => command_type,
    }
}

fn command_payload(parsed: &Value) -> Value {
    parsed
        .get("payload")
        .cloned()
        .unwrap_or_else(|| parsed.clone())
}

fn parse_command_payload<T>(parsed: &Value, command_type: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(command_payload(parsed))
        .map_err(|err| Error::validation(format!("Invalid payload for {command_type}: {err}")))
}

fn next_run_id(candidate: Option<&str>) -> String {
    candidate
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4().simple()))
}

async fn persist_runtime_snapshot(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    runtime_store: &RuntimeStore,
    snapshot: &RunSnapshot,
) -> Result<()> {
    runtime_store.save_snapshot(snapshot)?;

    let mut guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    {
        let mut inner_session = guard
            .session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        inner_session.append_custom_entry(
            "runtime_run_snapshot".to_string(),
            Some(json!(snapshot.clone())),
        );
    }
    guard.persist_session().await?;
    Ok(())
}

async fn persist_runtime_output(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    runtime_store: &RuntimeStore,
    output: &RuntimeControllerOutput,
) -> Result<()> {
    runtime_store.append_events(&output.events)?;
    persist_runtime_snapshot(cx, session, runtime_store, &output.snapshot).await
}

const fn runtime_task_state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Draft | TaskState::Blocked => "blocked",
        TaskState::Ready => "ready",
        TaskState::Leased | TaskState::Executing => "leased",
        TaskState::Verifying => "verifying",
        TaskState::AwaitingHuman => "awaiting_human",
        TaskState::Recoverable => "recoverable",
        TaskState::Succeeded | TaskState::Failed | TaskState::Canceled | TaskState::Superseded => {
            "terminal"
        }
    }
}

fn sync_runtime_task_from_reliability(
    runtime_task: &mut TaskNode,
    reliability_task: &reliability::TaskNode,
) {
    runtime_task
        .spec
        .objective
        .clone_from(&reliability_task.spec.objective);
    runtime_task.spec.planned_touches = reliability_task
        .spec
        .planned_touches
        .iter()
        .map(PathBuf::from)
        .collect();
    runtime_task.spec.input_snapshot = Some(reliability_task.spec.input_snapshot.clone());
    runtime_task.spec.max_attempts = reliability_task.spec.max_attempts;
    runtime_task
        .spec
        .constraints
        .invariants
        .clone_from(&reliability_task.spec.constraints.invariants);
    runtime_task.spec.constraints.max_touched_files =
        reliability_task.spec.constraints.max_touched_files;
    runtime_task
        .spec
        .constraints
        .forbid_paths
        .clone_from(&reliability_task.spec.constraints.forbid_paths);
    let reliability::VerifyPlan::Standard {
        command,
        timeout_sec,
        ..
    } = &reliability_task.spec.verify;
    runtime_task.spec.verify.command.clone_from(command);
    runtime_task.spec.verify.timeout_sec = *timeout_sec;
    runtime_task
        .spec
        .verify
        .acceptance_ids
        .clone_from(&reliability_task.spec.acceptance_ids);
    runtime_task.runtime.attempt = reliability_task.runtime.attempt;
    runtime_task.runtime.continuation_reason = None;
    runtime_task.runtime.last_error = None;

    match &reliability_task.runtime.state {
        reliability::RuntimeState::Blocked { .. } => {
            runtime_task.runtime.state = TaskState::Blocked;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
        }
        reliability::RuntimeState::Ready => {
            runtime_task.runtime.state = TaskState::Ready;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
        }
        reliability::RuntimeState::Leased {
            lease_id,
            agent_id,
            fence_token,
            expires_at,
        } => {
            runtime_task.runtime.state = TaskState::Leased;
            runtime_task.runtime.lease = Some(LeaseRecord {
                lease_id: lease_id.clone(),
                owner: agent_id.clone(),
                fence_token: *fence_token,
                expires_at: *expires_at,
            });
            runtime_task.runtime.retry_at = None;
        }
        reliability::RuntimeState::Verifying { .. } => {
            runtime_task.runtime.state = TaskState::Verifying;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
        }
        reliability::RuntimeState::Recoverable {
            reason,
            handoff_summary,
            retry_after,
            ..
        } => {
            runtime_task.runtime.state = TaskState::Recoverable;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = *retry_after;
            runtime_task.runtime.last_error = Some(crate::runtime::types::FailureRecord {
                code: failure_class_label(*reason).to_string(),
                message: handoff_summary.clone(),
                retry_at: *retry_after,
            });
        }
        reliability::RuntimeState::AwaitingHuman {
            question, context, ..
        } => {
            runtime_task.runtime.state = TaskState::AwaitingHuman;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
            runtime_task.runtime.last_error = Some(crate::runtime::types::FailureRecord {
                code: failure_class_label(reliability::FailureClass::HumanBlocker).to_string(),
                message: format!("{question}: {context}"),
                retry_at: None,
            });
        }
        reliability::RuntimeState::Terminal(reliability::TerminalState::Succeeded { .. }) => {
            runtime_task.runtime.state = TaskState::Succeeded;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
        }
        reliability::RuntimeState::Terminal(reliability::TerminalState::Failed {
            class,
            failed_at,
            ..
        }) => {
            runtime_task.runtime.state = TaskState::Failed;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
            runtime_task.runtime.last_error = Some(crate::runtime::types::FailureRecord {
                code: failure_class_label(*class).to_string(),
                message: format!("task failed at {}", failed_at.to_rfc3339()),
                retry_at: None,
            });
        }
        reliability::RuntimeState::Terminal(reliability::TerminalState::Superseded { .. }) => {
            runtime_task.runtime.state = TaskState::Superseded;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
        }
        reliability::RuntimeState::Terminal(reliability::TerminalState::Canceled {
            reason,
            ..
        }) => {
            runtime_task.runtime.state = TaskState::Canceled;
            runtime_task.runtime.lease = None;
            runtime_task.runtime.retry_at = None;
            runtime_task.runtime.last_error = Some(crate::runtime::types::FailureRecord {
                code: "canceled".to_string(),
                message: reason.clone(),
                retry_at: None,
            });
        }
    }
}

fn sync_runtime_snapshot_from_reliability(
    reliability: &RpcReliabilityState,
    snapshot: &mut RunSnapshot,
) {
    for (task_id, runtime_task) in &mut snapshot.tasks {
        let Some(reliability_task) = reliability.tasks.get(task_id) else {
            continue;
        };
        sync_runtime_task_from_reliability(runtime_task, reliability_task);
    }
    recompute_runtime_task_counts(snapshot);
    scheduler::rebuild_ready_queue(snapshot);
}

fn apply_runtime_scheduler_lifecycle(snapshot: &mut RunSnapshot) {
    let mut next_action = None;
    snapshot.wake_at = None;

    for event in scheduler::phase_and_wake_events(snapshot, Utc::now()) {
        match event {
            crate::runtime::events::RuntimeEventKind::PhaseChanged { phase, summary } => {
                snapshot.phase = phase;
                if next_action.is_none() {
                    next_action = summary;
                }
            }
            crate::runtime::events::RuntimeEventKind::WakeScheduled { wake_at, reason } => {
                snapshot.wake_at = Some(wake_at);
                if next_action.is_none() {
                    next_action = Some(reason);
                }
            }
            crate::runtime::events::RuntimeEventKind::RunCompleted => {
                snapshot.phase = RunPhase::Completed;
                next_action = Some("run completed".to_string());
            }
            crate::runtime::events::RuntimeEventKind::RunFailed { reason } => {
                snapshot.phase = RunPhase::Failed;
                next_action = Some(reason);
            }
            crate::runtime::events::RuntimeEventKind::RunCanceled { reason } => {
                snapshot.phase = RunPhase::Canceled;
                next_action = Some(reason);
            }
            _ => {}
        }
    }

    snapshot.summary.next_action = next_action;
}

const fn run_requires_plan_acceptance(snapshot: &RunSnapshot) -> bool {
    snapshot.plan_required && !snapshot.plan_accepted
}

fn recompute_runtime_task_counts(snapshot: &mut RunSnapshot) {
    let mut task_counts = BTreeMap::new();
    for task in snapshot.tasks.values() {
        *task_counts
            .entry(runtime_task_state_label(task.runtime.state).to_string())
            .or_insert(0) += 1;
    }
    snapshot.summary.task_counts = task_counts;
}

fn load_runtime_snapshot(runtime_store: &RuntimeStore, run_id: &str) -> Option<RunSnapshot> {
    runtime_store.load_snapshot(run_id).ok()
}

fn runtime_model_selector(
    entry: &ModelEntry,
    thinking_level: Option<crate::model::ThinkingLevel>,
) -> ModelSelector {
    let thinking_level = thinking_level
        .map(|level| entry.clamp_thinking_level(level))
        .filter(|level| *level != crate::model::ThinkingLevel::Off)
        .map(|level| level.to_string());
    ModelSelector {
        provider: entry.model.provider.clone(),
        model: entry.model.id.clone(),
        thinking_level,
    }
}

async fn runtime_model_profile_for_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    options: &RpcOptions,
) -> Result<ModelProfile> {
    let (current_entry, current_thinking) = {
        let guard = session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        let provider_entry = runtime_provider_model_entry(
            guard.agent.provider().name(),
            guard.agent.provider().model_id(),
        );
        let inner_session = guard
            .session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        let thinking = inner_session
            .header
            .thinking_level
            .as_deref()
            .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok());
        (
            runtime_selected_model_entry(&inner_session, options).or(provider_entry),
            thinking,
        )
    };

    let default_entry = current_entry
        .clone()
        .or_else(|| {
            options
                .scoped_models
                .first()
                .map(|scoped| scoped.model.clone())
        })
        .or_else(|| options.available_models.first().cloned())
        .ok_or_else(|| {
            Error::validation(
                "No available models configured for runtime orchestration".to_string(),
            )
        })?;

    let planner_entry = current_entry
        .clone()
        .filter(|entry| entry.model.reasoning)
        .or_else(|| {
            options
                .scoped_models
                .iter()
                .find(|scoped| scoped.model.model.reasoning)
                .map(|scoped| scoped.model.clone())
        })
        .or_else(|| {
            options
                .available_models
                .iter()
                .find(|entry| entry.model.reasoning)
                .cloned()
        })
        .unwrap_or_else(|| default_entry.clone());

    let background_entry = options
        .available_models
        .iter()
        .find(|entry| !entry.model.reasoning)
        .cloned()
        .unwrap_or_else(|| default_entry.clone());

    Ok(ModelProfile {
        planner: runtime_model_selector(
            &planner_entry,
            current_thinking.or(Some(crate::model::ThinkingLevel::High)),
        ),
        executor: runtime_model_selector(&default_entry, current_thinking),
        verifier: runtime_model_selector(&default_entry, current_thinking),
        summarizer: runtime_model_selector(&background_entry, None),
        background: runtime_model_selector(&background_entry, None),
    })
}

fn runtime_selected_model_entry(
    session: &crate::session::Session,
    options: &RpcOptions,
) -> Option<ModelEntry> {
    current_model_entry(session, options).cloned().or_else(|| {
        let provider = session.header.provider.as_deref()?;
        let model_id = session.header.model_id.as_deref()?;
        runtime_provider_model_entry(provider, model_id)
    })
}

fn runtime_provider_model_entry(provider: &str, model_id: &str) -> Option<ModelEntry> {
    let provider = provider.trim();
    let model_id = model_id.trim();
    if provider.is_empty() || model_id.is_empty() {
        return None;
    }

    if let Some(entry) = crate::models::ad_hoc_model_entry(provider, model_id) {
        return Some(entry);
    }

    let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);
    let defaults = provider_routing_defaults(canonical_provider);
    let display_name = provider_metadata(canonical_provider)
        .and_then(|metadata| metadata.display_name)
        .unwrap_or(canonical_provider);

    Some(ModelEntry {
        model: crate::provider::Model {
            id: model_id.to_string(),
            name: format!("{display_name} {model_id}"),
            api: defaults
                .map(|routing| routing.api)
                .unwrap_or("unknown")
                .to_string(),
            provider: canonical_provider.to_string(),
            base_url: defaults
                .map(|routing| routing.base_url)
                .unwrap_or_default()
                .to_string(),
            reasoning: defaults.map(|routing| routing.reasoning).unwrap_or(true),
            input: defaults
                .map(|routing| routing.input.to_vec())
                .unwrap_or_else(|| vec![crate::provider::InputType::Text]),
            cost: crate::provider::ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: defaults
                .map(|routing| routing.context_window)
                .unwrap_or(128_000),
            max_tokens: defaults.map(|routing| routing.max_tokens).unwrap_or(16_384),
            headers: HashMap::new(),
        },
        api_key: None,
        headers: HashMap::new(),
        auth_header: defaults.map(|routing| routing.auth_header).unwrap_or(false),
        compat: None,
        oauth_config: None,
    })
}

fn runtime_plan_digest(req: &StartRunRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(req.objective.as_bytes());
    hasher.update(req.run_verify_command.as_bytes());
    for task in &req.tasks {
        hasher.update(task.task_id.as_bytes());
        hasher.update(task.objective.as_bytes());
        hasher.update(task.verify_command.as_bytes());
        for prereq in &task.prerequisites {
            hasher.update(prereq.task_id.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

fn build_runtime_plan_artifact(run_id: &str, req: &StartRunRequest) -> PlanArtifact {
    let mut test_strategy = BTreeSet::new();
    test_strategy.insert(req.run_verify_command.clone());
    for task in &req.tasks {
        if !task.verify_command.trim().is_empty() {
            test_strategy.insert(task.verify_command.clone());
        }
    }

    PlanArtifact {
        plan_id: format!("{run_id}-plan"),
        digest: runtime_plan_digest(req),
        objective: req.objective.clone(),
        task_drafts: req
            .tasks
            .iter()
            .map(|task| format!("{}: {}", task.task_id, task.objective))
            .collect(),
        touched_paths: req
            .tasks
            .iter()
            .flat_map(|task| task.planned_touches.iter().map(PathBuf::from))
            .collect(),
        test_strategy: test_strategy.into_iter().collect(),
        evidence_refs: Vec::new(),
        produced_at: Utc::now(),
    }
}

fn build_runtime_task_nodes(
    req: &StartRunRequest,
    default_verify_timeout_sec: u32,
) -> Vec<TaskNode> {
    req.tasks
        .iter()
        .map(|task| {
            let verify_command = if task.verify_command.trim().is_empty() {
                req.run_verify_command.clone()
            } else {
                task.verify_command.clone()
            };
            let verify_timeout_sec = task
                .verify_timeout_sec
                .unwrap_or(default_verify_timeout_sec);
            let mut node = TaskNode::new(TaskSpec {
                task_id: task.task_id.clone(),
                title: task.task_id.clone(),
                objective: task.objective.clone(),
                parent_goal_trace_id: task.parent_goal_trace_id.clone(),
                planned_touches: task.planned_touches.iter().map(PathBuf::from).collect(),
                input_snapshot: task.input_snapshot.clone(),
                max_attempts: task.max_attempts.unwrap_or(1),
                enforce_symbol_drift_check: task.enforce_symbol_drift_check,
                verify: VerifySpec {
                    command: verify_command,
                    timeout_sec: verify_timeout_sec,
                    acceptance_ids: task.acceptance_ids.clone(),
                },
                autonomy: AutonomyLevel::Guarded,
                constraints: TaskConstraints {
                    invariants: task.invariants.clone(),
                    max_touched_files: task.max_touched_files,
                    forbid_paths: task.forbid_paths.clone(),
                },
            });
            node.deps = task
                .prerequisites
                .iter()
                .map(|prereq| prereq.task_id.clone())
                .collect();
            node
        })
        .collect()
}

async fn bootstrap_runtime_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    options: &RpcOptions,
    run_id: &str,
    req: &StartRunRequest,
) -> Result<RuntimeControllerOutput> {
    let model_profile = runtime_model_profile_for_run(cx, session, options).await?;
    let selected_tier = select_execution_tier(&req.tasks);
    let run_verify_timeout_sec = req
        .run_verify_timeout_sec
        .unwrap_or(options.config.reliability_verify_timeout_sec_default());
    let spec = RunSpec {
        run_id: run_id.to_string(),
        objective: req.objective.clone(),
        root_workspace: std::env::current_dir().map_err(|err| Error::Io(Box::new(err)))?,
        policy_profile: "default".to_string(),
        model_profile: "current_session".to_string(),
        run_verify_command: Some(req.run_verify_command.clone()),
        run_verify_timeout_sec: Some(run_verify_timeout_sec),
        budgets: RunBudgets {
            max_parallelism: req
                .max_parallelism
                .unwrap_or(RunBudgets::default().max_parallelism),
            max_steps: None,
            max_cost_microusd: None,
        },
        constraints: RunConstraints::default(),
        created_at: Utc::now(),
    };
    let plan = build_runtime_plan_artifact(run_id, req);
    let tasks = build_runtime_task_nodes(req, run_verify_timeout_sec);
    let route = ModelRoute::from_profile(&model_profile);
    let mut controller = RuntimeController::new(PhaseModelRouter::new(route), PolicySet::new());
    let mut output = controller
        .handle(RuntimeCommand::BootstrapRun { spec, plan, tasks })
        .map_err(|err| Error::session(format!("runtime bootstrap failed: {err}")))?;
    output.snapshot.dispatch.selected_tier = selected_tier;
    Ok(output)
}

async fn accept_runtime_plan(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    options: &RpcOptions,
    snapshot: RunSnapshot,
) -> Result<RuntimeControllerOutput> {
    let model_profile = runtime_model_profile_for_run(cx, session, options).await?;
    let route = ModelRoute::from_profile(&model_profile);
    let mut controller =
        RuntimeController::from_snapshot(snapshot, PhaseModelRouter::new(route), PolicySet::new());
    controller
        .handle(RuntimeCommand::AcceptPlan)
        .map_err(|err| Error::session(format!("plan acceptance failed: {err}")))
}

async fn append_dispatch_grants_session_entries(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    grants: &[DispatchGrant],
) -> Result<()> {
    if grants.is_empty() {
        return Ok(());
    }

    let tasks = {
        let reliability = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        grants
            .iter()
            .filter_map(|grant| {
                reliability
                    .tasks
                    .get(&grant.task_id)
                    .map(|task| (grant.clone(), task.spec.objective.clone()))
            })
            .collect::<Vec<_>>()
    };

    if tasks.is_empty() {
        return Ok(());
    }

    let mut guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    {
        let mut inner_session = guard
            .session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        for (grant, objective) in tasks {
            inner_session.append_task_created_entry(
                grant.task_id.clone(),
                objective,
                Some(grant.agent_id.clone()),
            );
            inner_session.append_task_transition_entry(
                grant.task_id,
                None,
                grant.state,
                Some(json!({
                    "leaseId": grant.lease_id,
                    "fenceToken": grant.fence_token,
                })),
            );
        }
    }
    guard.persist_session().await?;
    Ok(())
}

async fn append_canceled_dispatch_grants_session_entries(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    grants: &[DispatchGrant],
) -> Result<()> {
    if grants.is_empty() {
        return Ok(());
    }

    let transitions = {
        let reliability = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        grants
            .iter()
            .filter_map(|grant| {
                reliability.tasks.get(&grant.task_id).map(|task| {
                    (
                        grant.task_id.clone(),
                        grant.state.clone(),
                        RpcReliabilityState::state_label(&task.runtime.state).to_string(),
                        grant.lease_id.clone(),
                        grant.fence_token,
                    )
                })
            })
            .collect::<Vec<_>>()
    };

    if transitions.is_empty() {
        return Ok(());
    }

    let mut guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    {
        let mut inner_session = guard
            .session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        for (task_id, from, to, lease_id, fence_token) in transitions {
            inner_session.append_task_transition_entry(
                task_id,
                Some(from),
                to,
                Some(json!({
                    "leaseId": lease_id,
                    "fenceToken": fence_token,
                    "reason": "orchestration.cancel_run",
                })),
            );
        }
    }
    guard.persist_session().await?;
    Ok(())
}

async fn append_dispatch_rollback_session_entry(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    grant: &DispatchGrant,
    to_state: &str,
    summary: &str,
) -> Result<()> {
    let mut guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    {
        let mut inner_session = guard
            .session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        inner_session.append_task_transition_entry(
            grant.task_id.clone(),
            Some(grant.state.clone()),
            to_state.to_string(),
            Some(json!({
                "leaseId": grant.lease_id,
                "fenceToken": grant.fence_token,
                "reason": "orchestration.rollback_dispatch_grant",
                "summary": summary,
            })),
        );
    }
    guard.persist_session().await?;
    Ok(())
}

fn ensure_run_id_available(runtime_store: &RuntimeStore, run_id: &str) -> Result<()> {
    if runtime_store.exists(run_id) {
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

fn select_execution_tier(tasks: &[TaskContract]) -> ExecutionTier {
    match tasks.len() {
        0 | 1 => ExecutionTier::Inline,
        2..=24 if orchestration_dag_depth(tasks) <= 4 => ExecutionTier::Wave,
        _ => ExecutionTier::Hierarchical,
    }
}

const fn failure_class_label(class: reliability::FailureClass) -> &'static str {
    match class {
        reliability::FailureClass::VerificationFailed => "verification_failed",
        reliability::FailureClass::ScopeCreepDetected => "scope_creep_detected",
        reliability::FailureClass::MergeConflict => "merge_conflict",
        reliability::FailureClass::InfraTransient => "infra_transient",
        reliability::FailureClass::InfraPermanent => "infra_permanent",
        reliability::FailureClass::HumanBlocker => "human_blocker",
        reliability::FailureClass::MaxAttemptsExceeded => "max_attempts_exceeded",
    }
}

fn runtime_failure_class_label(state: &reliability::RuntimeState) -> Option<String> {
    match state {
        reliability::RuntimeState::Recoverable { reason, .. } => {
            Some(failure_class_label(*reason).to_string())
        }
        reliability::RuntimeState::AwaitingHuman { .. } => {
            Some(failure_class_label(reliability::FailureClass::HumanBlocker).to_string())
        }
        reliability::RuntimeState::Terminal(reliability::TerminalState::Failed {
            class, ..
        }) => Some(failure_class_label(*class).to_string()),
        _ => None,
    }
}

fn task_report_blockers(state: &reliability::RuntimeState) -> Vec<String> {
    match state {
        reliability::RuntimeState::Blocked { waiting_on } => waiting_on.clone(),
        reliability::RuntimeState::AwaitingHuman {
            question, context, ..
        } => {
            let mut blockers = vec![question.clone()];
            if !context.trim().is_empty() {
                blockers.push(context.clone());
            }
            blockers
        }
        reliability::RuntimeState::Recoverable {
            handoff_summary,
            retry_after,
            ..
        } => {
            let mut blockers = Vec::new();
            if !handoff_summary.trim().is_empty() {
                blockers.push(handoff_summary.clone());
            }
            if let Some(retry_after) = retry_after {
                blockers.push(format!("retry_after={}", retry_after.to_rfc3339()));
            }
            blockers
        }
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletedRunVerifyScope {
    scope_id: String,
    scope_kind: RunVerifyScopeKind,
    subrun_id: Option<String>,
}

fn task_terminal_success(reliability: &RpcReliabilityState, task_id: &str) -> bool {
    reliability.tasks.get(task_id).is_some_and(|task| {
        matches!(
            task.runtime.state,
            reliability::RuntimeState::Terminal(
                reliability::TerminalState::Succeeded { .. }
                    | reliability::TerminalState::Superseded { .. }
            )
        )
    })
}

fn run_terminal_success(reliability: &RpcReliabilityState, run: &RunSnapshot) -> bool {
    let task_ids = run.task_ids();
    !task_ids.is_empty()
        && task_ids
            .iter()
            .all(|task_id| task_terminal_success(reliability, task_id))
}

fn completed_run_verify_scope(
    reliability: &RpcReliabilityState,
    run: &RunSnapshot,
) -> Option<CompletedRunVerifyScope> {
    if run.run_verify_command().trim().is_empty() || !run_terminal_success(reliability, run) {
        return None;
    }
    Some(CompletedRunVerifyScope {
        scope_id: run.spec.run_id.clone(),
        scope_kind: RunVerifyScopeKind::Run,
        subrun_id: None,
    })
}

fn should_skip_run_verify(run: &RunSnapshot, scope: &CompletedRunVerifyScope) -> bool {
    run.dispatch
        .latest_run_verify
        .as_ref()
        .is_some_and(|status| {
            status.scope_id == scope.scope_id
                && status.scope_kind == scope.scope_kind
                && status.subrun_id == scope.subrun_id
        })
}

fn completed_scope_from_run_verify(status: &RunVerifyStatus) -> CompletedRunVerifyScope {
    CompletedRunVerifyScope {
        scope_id: status.scope_id.clone(),
        scope_kind: status.scope_kind,
        subrun_id: status.subrun_id.clone(),
    }
}

fn apply_run_verify_lifecycle(run: &mut RunSnapshot) {
    if !matches!(run.phase, RunPhase::Canceled)
        && run
            .dispatch
            .latest_run_verify
            .as_ref()
            .is_some_and(|status| !status.ok)
    {
        run.phase = RunPhase::Failed;
        run.touch();
    }
}

fn run_has_live_tasks(reliability: &RpcReliabilityState, run: &RunSnapshot) -> bool {
    let task_ids = run.task_ids();
    !task_ids.is_empty()
        && task_ids
            .iter()
            .all(|task_id| reliability.tasks.contains_key(task_id))
}

fn refresh_live_run_from_reliability(
    reliability: &mut RpcReliabilityState,
    run: &mut RunSnapshot,
) -> bool {
    let has_live_tasks = run_has_live_tasks(reliability, run);
    if has_live_tasks {
        reliability.refresh_dependency_states();
        refresh_run_from_reliability(reliability, run);
    }
    has_live_tasks
}

fn refresh_run_from_reliability(reliability: &RpcReliabilityState, run: &mut RunSnapshot) {
    let preserve_canceled = matches!(run.phase, RunPhase::Canceled);
    let task_ids = run.task_ids();
    sync_runtime_snapshot_from_reliability(reliability, run);
    run.dispatch.selected_tier = scheduler::execution_tier(run);
    run.dispatch.planned_subruns = scheduler::planned_subruns(run);

    if preserve_canceled || run_requires_plan_acceptance(run) {
        run.dispatch.active_subrun_id = None;
        run.dispatch.active_wave = None;
    } else {
        let active_scope_task_ids = if scheduler::execution_tier(run) == ExecutionTier::Hierarchical
        {
            let active_subrun = run.dispatch.planned_subruns.iter().find(|subrun| {
                subrun.task_ids.iter().any(|task_id| {
                    run.tasks
                        .get(task_id)
                        .is_some_and(|task| !task.runtime.state.is_terminal())
                })
            });
            run.dispatch.active_subrun_id = active_subrun.map(|subrun| subrun.subrun_id.clone());
            active_subrun
                .map(|subrun| subrun.task_ids.clone())
                .unwrap_or_default()
        } else {
            run.dispatch.active_subrun_id = None;
            task_ids.clone()
        };

        let mut active_task_ids = active_scope_task_ids
            .iter()
            .filter_map(|task_id| {
                let task = run.tasks.get(task_id)?;
                matches!(task.runtime.state, TaskState::Leased | TaskState::Verifying)
                    .then_some(task_id.clone())
            })
            .collect::<Vec<_>>();
        if active_task_ids.is_empty() {
            active_task_ids = scheduler::planned_wave_task_ids(run, &active_scope_task_ids);
        }
        run.dispatch.active_wave =
            scheduler::next_active_wave(run.dispatch.active_wave.take(), active_task_ids);
    }

    if run_requires_plan_acceptance(run) {
        run.phase = RunPhase::Planning;
        run.summary.next_action = Some("accept plan before dispatch".to_string());
        run.wake_at = None;
    } else if !preserve_canceled {
        apply_runtime_scheduler_lifecycle(run);
    }
    apply_run_verify_lifecycle(run);
    run.touch();
}

fn dispatch_run_wave(
    reliability: &mut RpcReliabilityState,
    run: &mut RunSnapshot,
    agent_id_prefix: &str,
    lease_ttl_sec: i64,
) -> Result<Vec<DispatchGrant>> {
    refresh_live_run_from_reliability(reliability, run);
    if matches!(
        run.phase,
        RunPhase::Canceled | RunPhase::Failed | RunPhase::Completed
    ) {
        return Err(Error::validation(format!(
            "run {} is not dispatchable in phase {:?}",
            run.spec.run_id, run.phase
        )));
    }

    let dispatchable_task_ids = run
        .dispatch
        .active_wave
        .as_ref()
        .map(|wave| {
            wave.task_ids
                .iter()
                .filter_map(|task_id| {
                    let task = reliability.tasks.get(task_id)?;
                    if matches!(task.runtime.state, reliability::RuntimeState::Ready) {
                        Some(task_id.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if dispatchable_task_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut grants = Vec::with_capacity(dispatchable_task_ids.len());
    for task_id in dispatchable_task_ids {
        let agent_id = format!("{agent_id_prefix}:{task_id}");
        match reliability.request_dispatch_existing(&task_id, &agent_id, lease_ttl_sec) {
            Ok(grant) => grants.push(grant),
            Err(err) => {
                for grant in &grants {
                    let _ = reliability.expire_dispatch_grant(grant);
                }
                reliability.refresh_dependency_states();
                refresh_run_from_reliability(reliability, run);
                return Err(err);
            }
        }
    }

    refresh_run_from_reliability(reliability, run);
    Ok(grants)
}

fn cancel_live_run_tasks(
    reliability: &mut RpcReliabilityState,
    run: &mut RunSnapshot,
) -> Vec<DispatchGrant> {
    let task_ids = run.task_ids();
    let leased_grants = task_ids
        .iter()
        .filter_map(|task_id| {
            let task = reliability.tasks.get(task_id)?;
            match &task.runtime.state {
                reliability::RuntimeState::Leased {
                    lease_id,
                    agent_id,
                    fence_token,
                    expires_at,
                } => Some(DispatchGrant {
                    task_id: task_id.clone(),
                    agent_id: agent_id.clone(),
                    lease_id: lease_id.clone(),
                    fence_token: *fence_token,
                    expires_at: *expires_at,
                    state: "leased".to_string(),
                }),
                _ => None,
            }
        })
        .collect::<Vec<_>>();

    for grant in &leased_grants {
        let _ = reliability.expire_dispatch_grant(grant);
    }

    reliability.refresh_dependency_states();
    refresh_run_from_reliability(reliability, run);
    run.phase = RunPhase::Canceled;
    run.dispatch.active_wave = None;
    run.dispatch.active_subrun_id = None;
    run.touch();
    leased_grants
}

fn build_cancel_run_task_report(
    reliability: &RpcReliabilityState,
    run_id: &str,
    task_id: &str,
) -> Option<TaskReport> {
    let task = reliability.tasks.get(task_id)?;
    let state = RpcReliabilityState::state_label(&task.runtime.state);
    build_runtime_task_report(
        reliability,
        task_id,
        format!("run {run_id} canceled dispatch for task {task_id}; task returned to {state}"),
    )
}

fn build_dispatch_rollback_task_report(
    reliability: &RpcReliabilityState,
    task_id: &str,
    summary: &str,
    failure_class: &str,
) -> Option<TaskReport> {
    let mut report = build_runtime_task_report(reliability, task_id, summary.to_string())?;
    report.summary = summary.to_string();
    report.failure_class = Some(failure_class.to_string());
    if reliability
        .evidence_by_task
        .get(task_id)
        .is_none_or(|evidence| evidence.is_empty())
    {
        report.verify_exit_code = -1;
    }
    Some(report)
}

async fn cancel_live_run_tasks_and_sync(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    run: &mut RunSnapshot,
) -> Result<()> {
    let canceled_grants = {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        cancel_live_run_tasks(&mut rel, run)
    };

    append_canceled_dispatch_grants_session_entries(
        cx,
        session,
        reliability_state,
        &canceled_grants,
    )
    .await?;

    for grant in canceled_grants {
        let report = {
            let rel = reliability_state
                .lock(cx)
                .await
                .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
            build_cancel_run_task_report(&rel, &run.spec.run_id, &grant.task_id)
        };
        let updated_runs = sync_task_runs(
            cx,
            session,
            reliability_state,
            runtime_store,
            &grant.task_id,
            report,
        )
        .await?;
        if let Some(updated_run) = updated_runs
            .into_iter()
            .find(|updated_run| updated_run.spec.run_id == run.spec.run_id)
        {
            *run = updated_run;
        }
    }

    run.phase = RunPhase::Canceled;
    run.dispatch.active_wave = None;
    run.dispatch.active_subrun_id = None;
    run.touch();
    persist_runtime_snapshot(cx, session, runtime_store, run).await?;

    Ok(())
}

const INLINE_WORKER_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];
const MAX_AUTOMATED_RECOVERABLE_WAIT: Duration = Duration::from_secs(60);
const ORCHESTRATION_ROLLBACK_RETRY_DELAY: Duration = Duration::from_millis(100);

fn next_recoverable_retry_delay(
    reliability: &RpcReliabilityState,
    run: &RunSnapshot,
) -> Option<Duration> {
    let now = chrono::Utc::now();

    run.task_ids()
        .iter()
        .filter_map(|task_id| reliability.tasks.get(task_id))
        .filter_map(|task| match &task.runtime.state {
            reliability::RuntimeState::Recoverable {
                retry_after: Some(retry_after),
                ..
            } if *retry_after > now => (*retry_after - now).to_std().ok(),
            _ => None,
        })
        .min()
}

fn apply_dispatch_rollback_recovery(
    reliability: &mut RpcReliabilityState,
    grant: &DispatchGrant,
    failure_summary: Option<&str>,
) -> String {
    let _ = reliability.expire_dispatch_grant(grant);

    let fallback_state = "ready".to_string();
    let rollback_state = {
        let Some(task) = reliability.tasks.get_mut(&grant.task_id) else {
            return fallback_state;
        };

        if let Some(summary) = failure_summary {
            let failed_at = chrono::Utc::now();
            task.runtime.attempt = task.runtime.attempt.saturating_add(1);
            task.runtime.last_transition_at = failed_at;
            if task.runtime.attempt < task.spec.max_attempts {
                let retry_after =
                    chrono::Duration::from_std(ORCHESTRATION_ROLLBACK_RETRY_DELAY).ok();
                task.runtime.state = reliability::RuntimeState::Recoverable {
                    reason: reliability::FailureClass::InfraTransient,
                    failure_artifact: None,
                    handoff_summary: summary.to_string(),
                    retry_after: retry_after.map(|delay| failed_at + delay),
                };
            } else {
                task.runtime.state =
                    reliability::RuntimeState::Terminal(reliability::TerminalState::Failed {
                        class: reliability::FailureClass::MaxAttemptsExceeded,
                        verify_run_id: None,
                        failed_at,
                    });
            }
        }

        RpcReliabilityState::state_label(&task.runtime.state).to_string()
    };

    reliability.refresh_dependency_states();
    reliability
        .tasks
        .get(&grant.task_id)
        .map(|task| RpcReliabilityState::state_label(&task.runtime.state).to_string())
        .unwrap_or(rollback_state)
}

#[derive(Debug)]
struct TaskVerificationOutcome {
    command: String,
    exit_code: i32,
    output: String,
    timed_out: bool,
    passed: bool,
}

#[allow(dead_code)]
#[async_trait]
trait OrchestrationInlineWorker: Send + Sync {
    async fn execute(&self, workspace_path: &Path, task: &TaskContract) -> Result<String>;
}

#[allow(dead_code)]
struct RpcSessionInlineWorker {
    provider: Arc<dyn Provider>,
    config: Config,
}

#[allow(dead_code)]
impl RpcSessionInlineWorker {
    fn new(provider: Arc<dyn Provider>, config: Config) -> Self {
        Self { provider, config }
    }
}

#[allow(dead_code)]
#[async_trait]
impl OrchestrationInlineWorker for RpcSessionInlineWorker {
    async fn execute(&self, workspace_path: &Path, task: &TaskContract) -> Result<String> {
        let tools = crate::tools::ToolRegistry::new(
            &INLINE_WORKER_TOOLS,
            workspace_path,
            Some(&self.config),
        );
        let agent = Agent::new(Arc::clone(&self.provider), tools, AgentConfig::default());
        let inner_session = Arc::new(Mutex::new(Session::create_with_dir(Some(
            workspace_path.to_path_buf(),
        ))));
        let mut session = AgentSession::new(
            agent,
            inner_session,
            false,
            ResolvedCompactionSettings::default(),
        );
        let prompt = automation_worker_prompt(task);
        let message = session.run_text(prompt, |_| {}).await?;
        Ok(assistant_message_summary(&message))
    }
}

#[allow(dead_code)]
fn assistant_message_summary(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(dead_code)]
fn automation_worker_prompt(task: &TaskContract) -> String {
    let invariants = if task.invariants.is_empty() {
        "none".to_string()
    } else {
        task.invariants.join("; ")
    };
    let forbidden = if task.forbid_paths.is_empty() {
        "none".to_string()
    } else {
        task.forbid_paths.join(", ")
    };
    let planned_touches = if task.planned_touches.is_empty() {
        "not specified; keep the diff minimal".to_string()
    } else {
        task.planned_touches.join(", ")
    };
    let acceptance = if task.acceptance_ids.is_empty() {
        "not specified".to_string()
    } else {
        task.acceptance_ids.join(", ")
    };
    format!(
        "You are executing one isolated orchestration task inside a git worktree.\n\
         Complete only the task below and keep the diff minimal.\n\n\
         Task ID: {task_id}\n\
         Objective: {objective}\n\
         Acceptance IDs: {acceptance}\n\
         Planned touches: {planned_touches}\n\
         Invariants: {invariants}\n\
         Forbidden paths: {forbidden}\n\
         Verify command: {verify_command}\n\n\
         Requirements:\n\
         - Stay inside the task scope.\n\
         - Prefer the built-in edit/write/bash/read tools.\n\
         - Run the verify command before finishing if possible.\n\
         - End with a short plain-language summary of what changed and whether verification passed.\n",
        task_id = task.task_id,
        objective = task.objective,
        acceptance = acceptance,
        planned_touches = planned_touches,
        invariants = invariants,
        forbidden = forbidden,
        verify_command = task.verify_command,
    )
}

#[allow(dead_code)]
fn patch_digest_for_diff(diff: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(diff.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

#[allow(dead_code)]
fn changed_files_from_diff(diff: &str) -> Vec<String> {
    let mut changed = Vec::new();
    for line in diff.lines() {
        if !line.starts_with("diff --git ") {
            continue;
        }
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 4 {
            continue;
        }
        let path = parts[2].strip_prefix("a/").unwrap_or(parts[2]).to_string();
        if !changed.contains(&path) {
            changed.push(path);
        }
    }
    changed
}

#[allow(dead_code)]
fn apply_diff_to_repo(repo_root: &Path, diff: &str) -> Result<()> {
    if diff.trim().is_empty() {
        return Ok(());
    }

    let mut child = Command::new("git")
        .current_dir(repo_root)
        .args(["apply", "--reject", "--whitespace=fix"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| Error::Io(Box::new(err)))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(diff.as_bytes())
            .map_err(|err| Error::Io(Box::new(err)))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|err| Error::Io(Box::new(err)))?;
    if output.status.success() {
        return Ok(());
    }

    Err(Error::tool(
        "orchestration",
        format!(
            "failed to apply worker diff: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    ))
}

#[allow(dead_code)]
fn repo_worktree_diff_against_snapshot(repo_root: &Path, snapshot: &str) -> Result<String> {
    let temp_dir = tempfile::tempdir().map_err(|err| Error::Io(Box::new(err)))?;
    let index_path = temp_dir.path().join("orchestration.index");

    let read_tree = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_INDEX_FILE", &index_path)
        .args(["read-tree", snapshot])
        .output()
        .map_err(|err| Error::Io(Box::new(err)))?;
    if !read_tree.status.success() {
        return Err(Error::tool(
            "orchestration",
            format!(
                "failed to seed temporary index from snapshot {snapshot}: {}",
                String::from_utf8_lossy(&read_tree.stderr).trim()
            ),
        ));
    }

    let add_all = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_INDEX_FILE", &index_path)
        .args(["add", "--all"])
        .output()
        .map_err(|err| Error::Io(Box::new(err)))?;
    if !add_all.status.success() {
        return Err(Error::tool(
            "orchestration",
            format!(
                "failed to capture parent worktree state: {}",
                String::from_utf8_lossy(&add_all.stderr).trim()
            ),
        ));
    }

    let diff_output = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_INDEX_FILE", &index_path)
        .args(["diff", "--cached", "--binary", snapshot])
        .output()
        .map_err(|err| Error::Io(Box::new(err)))?;
    if !diff_output.status.success() {
        return Err(Error::tool(
            "orchestration",
            format!(
                "failed to diff parent worktree against snapshot {snapshot}: {}",
                String::from_utf8_lossy(&diff_output.stderr).trim()
            ),
        ));
    }

    Ok(String::from_utf8_lossy(&diff_output.stdout).into_owned())
}

#[allow(dead_code)]
fn task_contract_for_runtime_task(
    reliability: &RpcReliabilityState,
    task_id: &str,
) -> Result<TaskContract> {
    let task = reliability
        .tasks
        .get(task_id)
        .ok_or_else(|| Error::validation(format!("Unknown reliability task: {task_id}")))?;
    let (verify_command, verify_timeout_sec) = match &task.spec.verify {
        reliability::VerifyPlan::Standard {
            command,
            timeout_sec,
            ..
        } => (command.clone(), Some(*timeout_sec)),
    };

    Ok(TaskContract {
        task_id: task.id.clone(),
        objective: task.spec.objective.clone(),
        parent_goal_trace_id: reliability.parent_goal_trace_by_task.get(task_id).cloned(),
        invariants: task.spec.constraints.invariants.clone(),
        max_touched_files: task.spec.constraints.max_touched_files,
        forbid_paths: task.spec.constraints.forbid_paths.clone(),
        verify_command,
        verify_timeout_sec,
        max_attempts: Some(task.spec.max_attempts),
        input_snapshot: Some(task.spec.input_snapshot.clone()),
        acceptance_ids: task.spec.acceptance_ids.clone(),
        planned_touches: task.spec.planned_touches.clone(),
        prerequisites: Vec::new(),
        enforce_symbol_drift_check: reliability
            .symbol_drift_required_by_task
            .get(task_id)
            .copied()
            .unwrap_or(false),
    })
}

#[allow(dead_code)]
async fn execute_task_verification(
    workspace_path: &Path,
    task: &TaskContract,
) -> TaskVerificationOutcome {
    let timeout_sec = task.verify_timeout_sec.unwrap_or(60);
    match Verifier::execute_verify_command(workspace_path, &task.verify_command, timeout_sec).await
    {
        Ok(execution) => {
            let passed = execution.passed();
            TaskVerificationOutcome {
                command: execution.command,
                exit_code: execution.exit_code,
                output: execution.output,
                timed_out: execution.cancelled,
                passed,
            }
        }
        Err(err) => TaskVerificationOutcome {
            command: task.verify_command.clone(),
            exit_code: -1,
            output: err.to_string(),
            timed_out: false,
            passed: false,
        },
    }
}

#[allow(dead_code)]
async fn append_evidence_record(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    req: AppendEvidenceRequest,
) -> Result<reliability::EvidenceRecord> {
    let evidence = {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        rel.append_evidence(req)?
    };

    let mut guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    {
        let mut inner_session = guard
            .session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
        inner_session.append_verification_evidence_entry(evidence.clone());
    }
    guard.persist_session().await?;
    Ok(evidence)
}

#[allow(dead_code)]
async fn submit_task_and_sync(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    req: SubmitTaskRequest,
) -> Result<SubmitTaskResponse> {
    let req_for_report = req.clone();
    let result = {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        rel.submit_task(req)?
    };

    {
        let mut guard = session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        {
            let mut inner_session = guard
                .session
                .lock(cx)
                .await
                .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
            inner_session
                .append_close_decision_entry(result.close_payload.clone(), result.close.clone());
            inner_session.append_task_transition_entry(
                result.task_id.clone(),
                None,
                result.state.clone(),
                None,
            );
        }
        guard.persist_session().await?;
    }

    let report = {
        let rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        build_submit_task_report(&rel, &req_for_report, &result)?
    };
    sync_task_runs(
        cx,
        session,
        reliability_state,
        runtime_store,
        &result.task_id,
        Some(report),
    )
    .await?;
    Ok(result)
}

#[allow(dead_code)]
async fn rollback_dispatch_grant(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    run_id: &str,
    grant: &DispatchGrant,
    failure_summary: Option<String>,
) {
    let (updated_run, rolled_back_state) = {
        let Ok(mut rel) = reliability_state.lock(cx).await else {
            return;
        };
        let rolled_back_state =
            apply_dispatch_rollback_recovery(&mut rel, grant, failure_summary.as_deref());
        let rollback_report = failure_summary.as_ref().and_then(|summary| {
            build_dispatch_rollback_task_report(
                &rel,
                &grant.task_id,
                summary,
                "orchestration_execution_error",
            )
        });
        let mut run = runtime_store.load_snapshot(run_id).ok();
        if let Some(mut run) = run.take() {
            if let Some(report) = rollback_report.as_ref() {
                run.upsert_task_report(report.clone());
            }
            refresh_run_from_reliability(&rel, &mut run);
            (Some(run), rolled_back_state)
        } else {
            (None, rolled_back_state)
        }
    };

    if let Some(summary) = failure_summary.as_deref() {
        let _ =
            append_dispatch_rollback_session_entry(cx, session, grant, &rolled_back_state, summary)
                .await;
    }

    if let Some(run) = updated_run {
        let _ = persist_runtime_snapshot(cx, session, runtime_store, &run).await;
    }
}

struct CapturedDispatchExecution {
    grant: DispatchGrant,
    contract: TaskContract,
    summary: String,
    diff: String,
    changed_files: Vec<String>,
    patch_digest: String,
    verification: TaskVerificationOutcome,
}

fn dispatch_workspace_segment_id(grant: &DispatchGrant) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    grant.task_id.hash(&mut hasher);
    grant.lease_id.hash(&mut hasher);
    grant.fence_token.hash(&mut hasher);
    hasher.finish() as usize
}

async fn capture_dispatch_grant_execution(
    cx: &AgentCx,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    repo_root: &Path,
    grant: &DispatchGrant,
    worker: &dyn OrchestrationInlineWorker,
) -> Result<CapturedDispatchExecution> {
    capture_dispatch_grant_execution_with_base_patches(
        cx,
        reliability_state,
        repo_root,
        grant,
        worker,
        &[],
    )
    .await
}

async fn capture_dispatch_grant_execution_with_base_patches(
    cx: &AgentCx,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    repo_root: &Path,
    grant: &DispatchGrant,
    worker: &dyn OrchestrationInlineWorker,
    base_patches: &[String],
) -> Result<CapturedDispatchExecution> {
    let contract = {
        let rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        task_contract_for_runtime_task(&rel, &grant.task_id)?
    };
    let snapshot = contract
        .input_snapshot
        .clone()
        .ok_or_else(|| Error::validation("inline orchestration task missing input snapshot"))?;
    let assigned_files = contract
        .planned_touches
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let mut workspace = FlockWorkspace::spawn_with_snapshot(
        repo_root,
        dispatch_workspace_segment_id(grant),
        assigned_files,
        snapshot.clone(),
    )?;
    workspace.prepare()?;
    let layered_base = if base_patches.is_empty() {
        let parent_base_patch = repo_worktree_diff_against_snapshot(repo_root, &snapshot)?;
        if parent_base_patch.trim().is_empty() {
            false
        } else {
            workspace.apply_patch(&parent_base_patch)?;
            true
        }
    } else {
        for patch in base_patches {
            workspace.apply_patch(patch)?;
        }
        true
    };
    if layered_base {
        workspace.commit_staged_changes("pi orchestration replay base")?;
    }

    let summary = worker.execute(workspace.workspace_path(), &contract).await;
    let diff = if layered_base {
        workspace.get_changes_since_head()
    } else {
        workspace.get_changes()
    };
    let verification = execute_task_verification(workspace.workspace_path(), &contract).await;
    let teardown_result = workspace.teardown();

    let summary = summary?;
    let diff = diff?;
    teardown_result?;

    Ok(CapturedDispatchExecution {
        grant: grant.clone(),
        contract,
        summary,
        changed_files: changed_files_from_diff(&diff),
        patch_digest: patch_digest_for_diff(&diff),
        diff,
        verification,
    })
}

fn captured_dispatches_overlap(captures: &[CapturedDispatchExecution]) -> bool {
    let mut changed_paths = HashSet::new();
    for capture in captures {
        for path in &capture.changed_files {
            if !changed_paths.insert(path.clone()) {
                return true;
            }
        }
    }
    false
}

async fn current_run_snapshot(
    _cx: &AgentCx,
    runtime_store: &RuntimeStore,
    run_id: &str,
) -> Result<RunSnapshot> {
    runtime_store.load_snapshot(run_id)
}

async fn finalize_captured_dispatch_execution(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    repo_root: &Path,
    run_id: &str,
    capture: CapturedDispatchExecution,
) -> Result<RunSnapshot> {
    let evidence = match append_evidence_record(
        cx,
        session,
        reliability_state,
        AppendEvidenceRequest {
            task_id: capture.contract.task_id.clone(),
            command: capture.verification.command.clone(),
            exit_code: capture.verification.exit_code,
            stdout: capture.verification.output.clone(),
            stderr: String::new(),
            artifact_ids: Vec::new(),
            env_id: Some("orchestration:auto".to_string()),
        },
    )
    .await
    {
        Ok(evidence) => evidence,
        Err(err) => {
            let summary = format!("failed to record orchestration evidence: {err}");
            rollback_dispatch_grant(
                cx,
                session,
                reliability_state,
                runtime_store,
                run_id,
                &capture.grant,
                Some(summary),
            )
            .await;
            return Err(err);
        }
    };

    if capture.verification.passed
        && let Err(err) = apply_diff_to_repo(repo_root, &capture.diff)
    {
        let summary = format!("failed to apply verified orchestration diff: {err}");
        rollback_dispatch_grant(
            cx,
            session,
            reliability_state,
            runtime_store,
            run_id,
            &capture.grant,
            Some(summary),
        )
        .await;
        return Err(err);
    }

    let trimmed_summary = capture.summary.trim();
    let close = capture
        .verification
        .passed
        .then(|| reliability::ClosePayload {
            task_id: capture.contract.task_id.clone(),
            outcome: if trimmed_summary.is_empty() {
                format!("Completed {}", capture.contract.objective)
            } else {
                trimmed_summary.to_string()
            },
            outcome_kind: Some(reliability::CloseOutcomeKind::Success),
            acceptance_ids: capture.contract.acceptance_ids.clone(),
            evidence_ids: vec![evidence.evidence_id.clone()],
            trace_parent: capture.contract.parent_goal_trace_id.clone(),
        });
    if let Err(err) = submit_task_and_sync(
        cx,
        session,
        reliability_state,
        runtime_store,
        SubmitTaskRequest {
            task_id: capture.grant.task_id.clone(),
            lease_id: capture.grant.lease_id.clone(),
            fence_token: capture.grant.fence_token,
            patch_digest: capture.patch_digest,
            verify_run_id: format!("orchestration-inline:{run_id}:{}", capture.grant.task_id),
            verify_passed: Some(capture.verification.passed),
            verify_timed_out: capture.verification.timed_out,
            failure_class: (!capture.verification.passed)
                .then_some(reliability::FailureClass::VerificationFailed),
            changed_files: capture.changed_files,
            symbol_drift_violations: Vec::new(),
            close,
        },
    )
    .await
    {
        let summary = format!("failed to submit orchestration task result: {err}");
        rollback_dispatch_grant(
            cx,
            session,
            reliability_state,
            runtime_store,
            run_id,
            &capture.grant,
            Some(summary),
        )
        .await;
        return Err(err);
    }

    current_run_snapshot(cx, runtime_store, run_id).await
}

#[allow(dead_code)]
async fn execute_inline_dispatch_grant_with_worker(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    repo_root: &Path,
    run_id: &str,
    grant: &DispatchGrant,
    worker: &dyn OrchestrationInlineWorker,
) -> Result<RunSnapshot> {
    let capture =
        match capture_dispatch_grant_execution(cx, reliability_state, repo_root, grant, worker)
            .await
        {
            Ok(capture) => capture,
            Err(err) => {
                let summary = format!("worker execution failed for {}: {err}", grant.task_id);
                rollback_dispatch_grant(
                    cx,
                    session,
                    reliability_state,
                    runtime_store,
                    run_id,
                    grant,
                    Some(summary),
                )
                .await;
                return Err(err);
            }
        };

    finalize_captured_dispatch_execution(
        cx,
        session,
        reliability_state,
        runtime_store,
        repo_root,
        run_id,
        capture,
    )
    .await
}

async fn execute_dispatch_grants_sequentially_with_replay_base(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    repo_root: &Path,
    run: RunSnapshot,
    grants: &[DispatchGrant],
    worker: &dyn OrchestrationInlineWorker,
) -> Result<RunSnapshot> {
    let mut updated_run = run;
    let mut replay_base_patches = Vec::new();

    for grant in grants {
        let capture = match capture_dispatch_grant_execution_with_base_patches(
            cx,
            reliability_state,
            repo_root,
            grant,
            worker,
            &replay_base_patches,
        )
        .await
        {
            Ok(capture) => capture,
            Err(err) => {
                let summary = format!(
                    "worker replay execution failed for {}: {err}",
                    grant.task_id
                );
                rollback_dispatch_grant(
                    cx,
                    session,
                    reliability_state,
                    runtime_store,
                    &updated_run.spec.run_id,
                    grant,
                    Some(summary),
                )
                .await;
                return Err(err);
            }
        };

        let replay_patch = capture.diff.clone();
        updated_run = finalize_captured_dispatch_execution(
            cx,
            session,
            reliability_state,
            runtime_store,
            repo_root,
            &updated_run.spec.run_id,
            capture,
        )
        .await?;

        if !replay_patch.trim().is_empty() {
            replay_base_patches.push(replay_patch);
        }
    }

    Ok(updated_run)
}

async fn finalize_captured_dispatches(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    repo_root: &Path,
    run: RunSnapshot,
    captures: Vec<CapturedDispatchExecution>,
) -> Result<RunSnapshot> {
    let mut updated_run = run;
    for capture in captures {
        updated_run = finalize_captured_dispatch_execution(
            cx,
            session,
            reliability_state,
            runtime_store,
            repo_root,
            &updated_run.spec.run_id,
            capture,
        )
        .await?;
    }
    Ok(updated_run)
}

async fn execute_dispatch_grants_with_worker(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    repo_root: &Path,
    run: RunSnapshot,
    grants: &[DispatchGrant],
    worker: &dyn OrchestrationInlineWorker,
) -> Result<RunSnapshot> {
    if grants.is_empty() {
        return Ok(run);
    }

    if grants.len() > 1 {
        let capture_results = futures::future::join_all(grants.iter().map(|grant| {
            capture_dispatch_grant_execution(cx, reliability_state, repo_root, grant, worker)
        }))
        .await;
        let mut captures = Vec::with_capacity(capture_results.len());
        let mut rollback_summaries = Vec::new();
        for (grant, result) in grants.iter().zip(capture_results) {
            match result {
                Ok(capture) => captures.push(capture),
                Err(err) => {
                    let failure_message = format!(
                        "parallel wave worker execution failed for {}: {err}",
                        grant.task_id
                    );
                    rollback_summaries.push((grant.clone(), failure_message));
                }
            }
        }

        if !rollback_summaries.is_empty() {
            for (grant, summary) in rollback_summaries {
                rollback_dispatch_grant(
                    cx,
                    session,
                    reliability_state,
                    runtime_store,
                    &run.spec.run_id,
                    &grant,
                    Some(summary),
                )
                .await;
            }

            let salvaged_count = captures.len();
            let updated_run = if salvaged_count > 0 {
                if captured_dispatches_overlap(&captures) {
                    let success_grants = captures
                        .iter()
                        .map(|capture| capture.grant.clone())
                        .collect::<Vec<_>>();
                    execute_dispatch_grants_sequentially_with_replay_base(
                        cx,
                        session,
                        reliability_state,
                        runtime_store,
                        repo_root,
                        run,
                        &success_grants,
                        worker,
                    )
                    .await?
                } else {
                    finalize_captured_dispatches(
                        cx,
                        session,
                        reliability_state,
                        runtime_store,
                        repo_root,
                        run,
                        captures,
                    )
                    .await?
                }
            } else {
                current_run_snapshot(cx, runtime_store, &run.spec.run_id).await?
            };
            return Ok(updated_run);
        }

        if captured_dispatches_overlap(&captures) {
            return execute_dispatch_grants_sequentially_with_replay_base(
                cx,
                session,
                reliability_state,
                runtime_store,
                repo_root,
                run,
                grants,
                worker,
            )
            .await;
        }

        return finalize_captured_dispatches(
            cx,
            session,
            reliability_state,
            runtime_store,
            repo_root,
            run,
            captures,
        )
        .await;
    }

    let mut updated_run = run;
    for grant in grants {
        updated_run = execute_inline_dispatch_grant_with_worker(
            cx,
            session,
            reliability_state,
            runtime_store,
            repo_root,
            &updated_run.spec.run_id,
            grant,
            worker,
        )
        .await?;
    }
    Ok(updated_run)
}

async fn execute_inline_run_dispatch(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    config: &Config,
    run: RunSnapshot,
    grants: &[DispatchGrant],
) -> Result<RunSnapshot> {
    let repo_root = session_workspace_root(cx, session).await?;
    let provider = {
        let guard = session
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
        guard.agent.provider()
    };
    let worker = RpcSessionInlineWorker::new(provider, config.clone());
    execute_dispatch_grants_with_worker(
        cx,
        session,
        reliability_state,
        runtime_store,
        repo_root.as_path(),
        run,
        grants,
        &worker,
    )
    .await
}

async fn session_workspace_root(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
) -> Result<PathBuf> {
    let guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    let inner = guard
        .session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    let cwd = inner.header.cwd.trim();
    Ok(if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    })
}

async fn dispatch_run_until_quiescent(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    config: &Config,
    mut run: RunSnapshot,
    agent_id_prefix: &str,
    lease_ttl_sec: i64,
) -> Result<(RunSnapshot, Vec<DispatchGrant>)> {
    loop {
        let mut grants = {
            let mut rel = reliability_state
                .lock(cx)
                .await
                .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
            let task_ids = run.task_ids();
            let has_live_tasks = !task_ids.is_empty()
                && task_ids
                    .iter()
                    .all(|task_id| rel.tasks.contains_key(task_id));
            if has_live_tasks {
                dispatch_run_wave(&mut rel, &mut run, agent_id_prefix, lease_ttl_sec)
            } else {
                Err(Error::session(format!(
                    "orchestration run {} is not live in this process",
                    run.spec.run_id
                )))
            }
        }?;

        if grants.is_empty() {
            let recoverable_wait = {
                let rel = reliability_state
                    .lock(cx)
                    .await
                    .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
                if run_has_live_tasks(&rel, &run)
                    && matches!(run.phase, RunPhase::Dispatching | RunPhase::Running)
                {
                    next_recoverable_retry_delay(&rel, &run)
                } else {
                    None
                }
            };
            if let Some(wait_for) =
                recoverable_wait.filter(|wait_for| *wait_for <= MAX_AUTOMATED_RECOVERABLE_WAIT)
            {
                cx.time().sleep(wait_for).await;
                continue;
            }
            return Ok((run, grants));
        }

        append_dispatch_grants_session_entries(cx, session, reliability_state, &grants).await?;
        let run_id = run.spec.run_id.clone();
        run = match execute_inline_run_dispatch(
            cx,
            session,
            reliability_state,
            runtime_store,
            config,
            run,
            &grants,
        )
        .await
        {
            Ok(updated_run) => updated_run,
            Err(err) if grants.len() == 1 => {
                let recovered_run = current_run_snapshot(cx, runtime_store, &run_id).await?;
                if matches!(
                    recovered_run.phase,
                    RunPhase::Dispatching | RunPhase::Running
                ) {
                    grants.clear();
                    run = recovered_run;
                    continue;
                }
                drop(err);
                recovered_run
            }
            Err(err) => return Err(err),
        };
        grants.clear();

        if matches!(
            run.phase,
            RunPhase::Canceled | RunPhase::Failed | RunPhase::Completed | RunPhase::AwaitingHuman
        ) {
            return Ok((run, grants));
        }
    }
}

fn build_submit_task_report(
    reliability: &RpcReliabilityState,
    req: &SubmitTaskRequest,
    result: &SubmitTaskResponse,
) -> Result<TaskReport> {
    let task = reliability.tasks.get(&result.task_id).ok_or_else(|| {
        Error::validation(format!("Unknown reliability task: {}", result.task_id))
    })?;

    let evidence = reliability
        .evidence_by_task
        .get(&result.task_id)
        .cloned()
        .unwrap_or_default();
    let verify_exit_code = evidence
        .last()
        .map(|record| record.exit_code)
        .unwrap_or_else(|| {
            if req.verify_timed_out {
                124
            } else {
                i32::from(!req.verify_passed.unwrap_or(result.close.approved))
            }
        });

    let attempt = match &task.runtime.state {
        reliability::RuntimeState::Terminal(reliability::TerminalState::Succeeded { .. }) => {
            task.runtime.attempt.saturating_add(1).max(1)
        }
        _ => task.runtime.attempt.max(1),
    };
    let summary = if result.close.approved {
        result.close_payload.outcome.clone()
    } else if result.close.violations.is_empty() {
        format!("task {} closed without approval", result.task_id)
    } else {
        result.close.violations.join("; ")
    };

    let verify_command = match &task.spec.verify {
        reliability::VerifyPlan::Standard { command, .. } => command.clone(),
    };

    Ok(TaskReport {
        task_id: result.task_id.clone(),
        attempt,
        summary,
        changed_files: req.changed_files.clone(),
        patch_digest: req.patch_digest.clone(),
        evidence_ids: result.close_payload.evidence_ids.clone(),
        acceptance_ids: if result.close_payload.acceptance_ids.is_empty() {
            task.spec.acceptance_ids.clone()
        } else {
            result.close_payload.acceptance_ids.clone()
        },
        verify_command,
        verify_exit_code,
        failure_class: runtime_failure_class_label(&task.runtime.state).or_else(|| {
            req.failure_class
                .map(|class| failure_class_label(class).to_string())
        }),
        blockers: task_report_blockers(&task.runtime.state),
        workspace_snapshot: task.spec.input_snapshot.clone(),
        generated_at: chrono::Utc::now(),
    })
}

fn build_runtime_task_report(
    reliability: &RpcReliabilityState,
    task_id: &str,
    summary: String,
) -> Option<TaskReport> {
    let task = reliability.tasks.get(task_id)?;
    let evidence = reliability
        .evidence_by_task
        .get(task_id)
        .cloned()
        .unwrap_or_default();
    let verify_command = match &task.spec.verify {
        reliability::VerifyPlan::Standard { command, .. } => command.clone(),
    };
    Some(TaskReport {
        task_id: task_id.to_string(),
        attempt: task.runtime.attempt.max(1),
        summary,
        changed_files: Vec::new(),
        patch_digest: String::new(),
        evidence_ids: evidence
            .iter()
            .map(|record| record.evidence_id.clone())
            .collect(),
        acceptance_ids: task.spec.acceptance_ids.clone(),
        verify_command,
        verify_exit_code: evidence.last().map(|record| record.exit_code).unwrap_or(0),
        failure_class: runtime_failure_class_label(&task.runtime.state),
        blockers: task_report_blockers(&task.runtime.state),
        workspace_snapshot: task.spec.input_snapshot.clone(),
        generated_at: chrono::Utc::now(),
    })
}

fn refresh_task_runs_with_verify_scopes(
    reliability: &RpcReliabilityState,
    runtime_store: &RuntimeStore,
    task_id: &str,
    report: Option<&TaskReport>,
) -> Vec<(RunSnapshot, Option<CompletedRunVerifyScope>)> {
    let run_ids = runtime_store
        .find_run_ids_by_task(task_id)
        .unwrap_or_default();
    let mut updated_runs = Vec::new();

    for run_id in run_ids {
        let Ok(mut run) = runtime_store.load_snapshot(&run_id) else {
            continue;
        };
        if let Some(report) = report.filter(|report| run.tasks.contains_key(&report.task_id)) {
            run.upsert_task_report(report.clone());
        }
        let verify_scope = completed_run_verify_scope(reliability, &run)
            .filter(|scope| !should_skip_run_verify(&run, scope));
        refresh_run_from_reliability(reliability, &mut run);
        updated_runs.push((run, verify_scope));
    }

    updated_runs
}

fn refresh_task_runs(
    reliability: &RpcReliabilityState,
    runtime_store: &RuntimeStore,
    task_id: &str,
    report: Option<TaskReport>,
) -> Vec<RunSnapshot> {
    refresh_task_runs_with_verify_scopes(reliability, runtime_store, task_id, report.as_ref())
        .into_iter()
        .map(|(run, _)| run)
        .collect()
}

fn run_verify_scope_summary(
    scope: &CompletedRunVerifyScope,
    ok: bool,
    details: impl AsRef<str>,
) -> String {
    let scope_label = match scope.scope_kind {
        RunVerifyScopeKind::Run => "run",
        RunVerifyScopeKind::Wave => "wave",
        RunVerifyScopeKind::Subrun => "subrun",
    };
    let prefix = if ok {
        "run verification passed"
    } else {
        "run verification failed"
    };
    format!(
        "{prefix} for {scope_label} {}: {}",
        scope.scope_id,
        details.as_ref().trim()
    )
}

async fn execute_run_verification(
    cwd: &Path,
    run: &mut RunSnapshot,
    scope: &CompletedRunVerifyScope,
) {
    let timeout_sec = run.spec.run_verify_timeout_sec.unwrap_or(60);
    let verify_status =
        match Verifier::execute_verify_command(cwd, run.run_verify_command(), timeout_sec).await {
            Ok(execution) => {
                let outcome = Verifier::classify_execution(&execution);
                let details = if outcome.ok {
                    format!(
                        "exit_code={}, duration_ms={}",
                        execution.exit_code, execution.duration_ms
                    )
                } else {
                    outcome.violations.join("; ")
                };
                RunVerifyStatus {
                    scope_id: scope.scope_id.clone(),
                    scope_kind: scope.scope_kind,
                    subrun_id: scope.subrun_id.clone(),
                    command: execution.command,
                    timeout_sec: execution.timeout_sec,
                    exit_code: execution.exit_code,
                    ok: outcome.ok,
                    summary: run_verify_scope_summary(scope, outcome.ok, details),
                    duration_ms: execution.duration_ms,
                    generated_at: chrono::Utc::now(),
                }
            }
            Err(err) => RunVerifyStatus {
                scope_id: scope.scope_id.clone(),
                scope_kind: scope.scope_kind,
                subrun_id: scope.subrun_id.clone(),
                command: run.run_verify_command().to_string(),
                timeout_sec,
                exit_code: -1,
                ok: false,
                summary: run_verify_scope_summary(scope, false, err.to_string()),
                duration_ms: 0,
                generated_at: chrono::Utc::now(),
            },
        };

    run.dispatch.latest_run_verify = Some(verify_status);
    apply_run_verify_lifecycle(run);
    run.touch();
}

async fn sync_task_runs(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    runtime_store: &RuntimeStore,
    task_id: &str,
    report: Option<TaskReport>,
) -> Result<Vec<RunSnapshot>> {
    let refreshed_runs = {
        let reliability = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        refresh_task_runs_with_verify_scopes(&reliability, runtime_store, task_id, report.as_ref())
    };

    let cwd = session_workspace_root(cx, session).await?;
    let mut updated_runs = Vec::with_capacity(refreshed_runs.len());
    for (mut run, verify_scope) in refreshed_runs {
        if let Some(scope) = verify_scope {
            execute_run_verification(&cwd, &mut run, &scope).await;
        }
        persist_runtime_snapshot(cx, session, runtime_store, &run).await?;
        updated_runs.push(run);
    }
    Ok(updated_runs)
}

fn build_user_message(text: &str, images: &[ImageContent]) -> Message {
    let timestamp = chrono::Utc::now().timestamp_millis();
    if images.is_empty() {
        return Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp,
        });
    }
    let mut blocks = vec![ContentBlock::Text(TextContent::new(text.to_string()))];
    for image in images {
        blocks.push(ContentBlock::Image(image.clone()));
    }
    Message::User(UserMessage {
        content: UserContent::Blocks(blocks),
        timestamp,
    })
}

fn is_extension_command(message: &str, expanded: &str) -> bool {
    // Extension commands start with `/` but are not expanded by the resource loader
    // (skills and prompt templates are expanded before queueing/sending).
    message.trim_start().starts_with('/') && message == expanded
}

fn try_send_line_with_backpressure(tx: &mpsc::Sender<String>, mut line: String) -> bool {
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

#[derive(Debug)]
struct RpcSharedState {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    auto_compaction_enabled: bool,
    auto_retry_enabled: bool,
}

const MAX_RPC_PENDING_MESSAGES: usize = 128;

impl RpcSharedState {
    fn new(config: &Config) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode: config.steering_queue_mode(),
            follow_up_mode: config.follow_up_queue_mode(),
            auto_compaction_enabled: config.compaction_enabled(),
            auto_retry_enabled: config.retry_enabled(),
        }
    }

    fn pending_count(&self) -> usize {
        self.steering.len() + self.follow_up.len()
    }

    fn push_steering(&mut self, message: Message) -> Result<()> {
        if self.steering.len() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session(
                "Steering queue is full (Do you have too many pending commands?)",
            ));
        }
        self.steering.push_back(message);
        Ok(())
    }

    fn push_follow_up(&mut self, message: Message) -> Result<()> {
        if self.follow_up.len() >= MAX_RPC_PENDING_MESSAGES {
            return Err(Error::session("Follow-up queue is full"));
        }
        self.follow_up.push_back(message);
        Ok(())
    }

    fn pop_steering(&mut self) -> Vec<Message> {
        match self.steering_mode {
            QueueMode::All => self.steering.drain(..).collect(),
            QueueMode::OneAtATime => self.steering.pop_front().into_iter().collect(),
        }
    }

    fn pop_follow_up(&mut self) -> Vec<Message> {
        match self.follow_up_mode {
            QueueMode::All => self.follow_up.drain(..).collect(),
            QueueMode::OneAtATime => self.follow_up.pop_front().into_iter().collect(),
        }
    }
}

async fn sync_agent_queue_modes(
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
struct RunningBash {
    id: String,
    abort_tx: oneshot::Sender<()>,
}

#[derive(Debug, Default)]
struct RpcUiBridgeState {
    active: Option<ExtensionUiRequest>,
    queue: VecDeque<ExtensionUiRequest>,
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
                    // Retry loop to handle backpressure (channel full) without dropping input.
                    // Stop when the receiver side has closed so this thread does not spin forever.
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
    let runtime_store = RuntimeStore::from_global_dir();

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

    // Set up extension UI channel for RPC mode.
    // When extensions request UI (capability prompts, etc.), we emit them as
    // JSON notifications so the RPC client can respond programmatically.
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
                    // Fire-and-forget UI updates should not be queued.
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

        match command_type {
            "prompt" => {
                let Some(message) = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
                else {
                    let resp = response_error(id, "prompt", "Missing message".to_string());
                    let _ = out_tx.send(resp);
                    continue;
                };

                let images = match parse_prompt_images(parsed.get("images")) {
                    Ok(images) => images,
                    Err(err) => {
                        let resp = response_error_with_hints(id, "prompt", &err);
                        let _ = out_tx.send(resp);
                        continue;
                    }
                };

                let streaming_behavior =
                    match parse_streaming_behavior(parsed.get("streamingBehavior")) {
                        Ok(value) => value,
                        Err(err) => {
                            let resp = response_error_with_hints(id, "prompt", &err);
                            let _ = out_tx.send(resp);
                            continue;
                        }
                    };

                let expanded = options.resources.expand_input(&message);

                if is_streaming.load(Ordering::SeqCst) {
                    if streaming_behavior.is_none() {
                        let resp = response_error(
                            id,
                            "prompt",
                            "Agent is currently streaming; specify streamingBehavior".to_string(),
                        );
                        let _ = out_tx.send(resp);
                        continue;
                    }

                    let queued_result = {
                        let mut state = shared_state
                            .lock(&cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        match streaming_behavior {
                            Some(StreamingBehavior::Steer) => {
                                state.push_steering(build_user_message(&expanded, &images))
                            }
                            Some(StreamingBehavior::FollowUp) => {
                                state.push_follow_up(build_user_message(&expanded, &images))
                            }
                            None => Ok(()), // Unreachable due to check above
                        }
                    };

                    match queued_result {
                        Ok(()) => {
                            let _ = out_tx.send(response_ok(id, "prompt", None));
                        }
                        Err(err) => {
                            let resp = response_error_with_hints(id, "prompt", &err);
                            let _ = out_tx.send(resp);
                        }
                    }
                    continue;
                }

                // Ack immediately.
                let _ = out_tx.send(response_ok(id, "prompt", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort = retry_abort.clone();
                let options = options.clone();
                let expanded = expanded.clone();
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

            "steer" => {
                let Some(message) = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
                else {
                    let resp = response_error(id, "steer", "Missing message".to_string());
                    let _ = out_tx.send(resp);
                    continue;
                };

                let expanded = options.resources.expand_input(&message);
                if is_extension_command(&message, &expanded) {
                    let resp = response_error(
                        id,
                        "steer",
                        "Extension commands are not allowed with steer".to_string(),
                    );
                    let _ = out_tx.send(resp);
                    continue;
                }

                if is_streaming.load(Ordering::SeqCst) {
                    let result = shared_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?
                        .push_steering(build_user_message(&expanded, &[]));

                    match result {
                        Ok(()) => {
                            let _ = out_tx.send(response_ok(id, "steer", None));
                        }
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(id, "steer", &err));
                        }
                    }
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "steer", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort = retry_abort.clone();
                let options = options.clone();
                let expanded = expanded.clone();
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
                        Vec::new(),
                        cx,
                    )
                    .await;
                });
            }

            "follow_up" => {
                let Some(message) = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .map(String::from)
                else {
                    let resp = response_error(id, "follow_up", "Missing message".to_string());
                    let _ = out_tx.send(resp);
                    continue;
                };

                let expanded = options.resources.expand_input(&message);
                if is_extension_command(&message, &expanded) {
                    let resp = response_error(
                        id,
                        "follow_up",
                        "Extension commands are not allowed with follow_up".to_string(),
                    );
                    let _ = out_tx.send(resp);
                    continue;
                }

                if is_streaming.load(Ordering::SeqCst) {
                    let result = shared_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?
                        .push_follow_up(build_user_message(&expanded, &[]));

                    match result {
                        Ok(()) => {
                            let _ = out_tx.send(response_ok(id, "follow_up", None));
                        }
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(id, "follow_up", &err));
                        }
                    }
                    continue;
                }

                let _ = out_tx.send(response_ok(id, "follow_up", None));

                is_streaming.store(true, Ordering::SeqCst);

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let shared_state = Arc::clone(&shared_state);
                let is_streaming = Arc::clone(&is_streaming);
                let is_compacting = Arc::clone(&is_compacting);
                let abort_handle_slot = Arc::clone(&abort_handle);
                let retry_abort = retry_abort.clone();
                let options = options.clone();
                let expanded = expanded.clone();
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
                        Vec::new(),
                        cx,
                    )
                    .await;
                });
            }

            "abort" => {
                let handle = abort_handle
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("abort lock failed: {err}")))?
                    .clone();
                if let Some(handle) = handle {
                    handle.abort();
                }
                let _ = out_tx.send(response_ok(id, "abort", None));
            }

            "get_state" => {
                let snapshot = {
                    let state = shared_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    RpcStateSnapshot::from(&*state)
                };
                let data = {
                    let inner_session = session_handle.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    session_state(
                        &inner_session,
                        &options,
                        &snapshot,
                        is_streaming.load(Ordering::SeqCst),
                        is_compacting.load(Ordering::SeqCst),
                    )
                };
                let _ = out_tx.send(response_ok(id, "get_state", Some(data)));
            }

            "orchestration.start_run" => {
                let req: StartRunRequest =
                    match parse_command_payload(&parsed, "orchestration.start_run") {
                        Ok(req) => req,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.start_run",
                                &err,
                            ));
                            continue;
                        }
                    };
                if req.tasks.is_empty() {
                    let _ = out_tx.send(response_error(
                        id,
                        "orchestration.start_run",
                        "Run must include at least one task".to_string(),
                    ));
                    continue;
                }
                if req.objective.trim().is_empty() {
                    let _ = out_tx.send(response_error(
                        id,
                        "orchestration.start_run",
                        "Run objective cannot be empty".to_string(),
                    ));
                    continue;
                }
                if req.run_verify_command.trim().is_empty() {
                    let _ = out_tx.send(response_error(
                        id,
                        "orchestration.start_run",
                        "runVerifyCommand cannot be empty".to_string(),
                    ));
                    continue;
                }
                if matches!(req.run_verify_timeout_sec, Some(0)) {
                    let _ = out_tx.send(response_error(
                        id,
                        "orchestration.start_run",
                        "runVerifyTimeoutSec must be at least 1 when provided".to_string(),
                    ));
                    continue;
                }
                if matches!(req.max_parallelism, Some(0)) {
                    let _ = out_tx.send(response_error(
                        id,
                        "orchestration.start_run",
                        "maxParallelism must be at least 1 when provided".to_string(),
                    ));
                    continue;
                }

                let run_id = next_run_id(req.run_id.as_deref());
                {
                    if let Err(err) = ensure_run_id_available(&runtime_store, &run_id) {
                        let _ = out_tx.send(response_error_with_hints(
                            id,
                            "orchestration.start_run",
                            &err,
                        ));
                        continue;
                    }
                }
                let result = {
                    let mut rel = reliability_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
                    for contract in &req.tasks {
                        rel.get_or_create_task(contract)?;
                    }
                    for contract in &req.tasks {
                        rel.reconcile_prerequisites(contract)?;
                    }
                    rel.refresh_dependency_states();
                    Ok::<(), Error>(())
                };
                if let Err(err) = result {
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.start_run",
                        &err,
                    ));
                    continue;
                }

                let runtime_bootstrap =
                    match bootstrap_runtime_run(&cx, &session, &options, &run_id, &req).await {
                        Ok(output) => output,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.start_run",
                                &err,
                            ));
                            continue;
                        }
                    };
                if let Err(err) =
                    persist_runtime_output(&cx, &session, &runtime_store, &runtime_bootstrap).await
                {
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.start_run",
                        &err,
                    ));
                    continue;
                }

                {
                    let run = runtime_bootstrap.snapshot;
                    if let Err(err) =
                        persist_runtime_snapshot(&cx, &session, &runtime_store, &run).await
                    {
                        let _ = out_tx.send(response_error_with_hints(
                            id,
                            "orchestration.start_run",
                            &err,
                        ));
                        continue;
                    }
                    let _ = out_tx.send(response_ok(
                        id,
                        "orchestration.start_run",
                        Some(json!({ "run": run })),
                    ));
                }
            }

            "orchestration.get_run" => {
                let req: RunLookupRequest =
                    match parse_command_payload(&parsed, "orchestration.get_run") {
                        Ok(req) => req,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.get_run",
                                &err,
                            ));
                            continue;
                        }
                    };

                if let Some(run) = load_runtime_snapshot(&runtime_store, &req.run_id) {
                    let _ = out_tx.send(response_ok(
                        id,
                        "orchestration.get_run",
                        Some(json!({ "run": run })),
                    ));
                } else {
                    let err =
                        Error::session(format!("orchestration run not found: {}", req.run_id));
                    let _ =
                        out_tx.send(response_error_with_hints(id, "orchestration.get_run", &err));
                }
            }

            "orchestration.accept_plan" => {
                let req: RunLookupRequest =
                    match parse_command_payload(&parsed, "orchestration.accept_plan") {
                        Ok(req) => req,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.accept_plan",
                                &err,
                            ));
                            continue;
                        }
                    };

                let run = if let Some(run) = load_runtime_snapshot(&runtime_store, &req.run_id) {
                    run
                } else {
                    let err =
                        Error::session(format!("orchestration run not found: {}", req.run_id));
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.accept_plan",
                        &err,
                    ));
                    continue;
                };

                if run.plan.is_none() {
                    let err = Error::session(format!(
                        "orchestration run {} does not have a materialized plan",
                        req.run_id
                    ));
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.accept_plan",
                        &err,
                    ));
                    continue;
                }

                let output = if run.plan_accepted {
                    RuntimeControllerOutput {
                        snapshot: run,
                        events: Vec::new(),
                        transitions: Vec::new(),
                        routed_phases: Vec::new(),
                        policy_decisions: Vec::new(),
                    }
                } else {
                    match accept_runtime_plan(&cx, &session, &options, run).await {
                        Ok(output) => output,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.accept_plan",
                                &err,
                            ));
                            continue;
                        }
                    }
                };

                if let Err(err) =
                    persist_runtime_output(&cx, &session, &runtime_store, &output).await
                {
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.accept_plan",
                        &err,
                    ));
                    continue;
                }

                let _ = out_tx.send(response_ok(
                    id,
                    "orchestration.accept_plan",
                    Some(json!({ "run": output.snapshot })),
                ));
            }

            "orchestration.dispatch_run" => {
                let req: DispatchRunRequest =
                    match parse_command_payload(&parsed, "orchestration.dispatch_run") {
                        Ok(req) => req,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.dispatch_run",
                                &err,
                            ));
                            continue;
                        }
                    };
                let agent_id_prefix = req
                    .agent_id_prefix
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("run");
                let lease_ttl_sec = req.lease_ttl_sec.unwrap_or(3600);

                let run = if let Some(run) = load_runtime_snapshot(&runtime_store, &req.run_id) {
                    run
                } else {
                    let err =
                        Error::session(format!("orchestration run not found: {}", req.run_id));
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.dispatch_run",
                        &err,
                    ));
                    continue;
                };

                if run_requires_plan_acceptance(&run) {
                    let err = Error::validation(format!(
                        "Run {} is still in planning; accept the plan before dispatching",
                        run.spec.run_id
                    ));
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.dispatch_run",
                        &err,
                    ));
                    continue;
                }

                let (run, grants) = match Box::pin(dispatch_run_until_quiescent(
                    &cx,
                    &session,
                    &reliability_state,
                    &runtime_store,
                    &options.config,
                    run,
                    agent_id_prefix,
                    lease_ttl_sec,
                ))
                .await
                {
                    Ok(result) => result,
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(
                            id,
                            "orchestration.dispatch_run",
                            &err,
                        ));
                        continue;
                    }
                };
                if let Err(err) =
                    persist_runtime_snapshot(&cx, &session, &runtime_store, &run).await
                {
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.dispatch_run",
                        &err,
                    ));
                    continue;
                }

                let _ = out_tx.send(response_ok(
                    id,
                    "orchestration.dispatch_run",
                    Some(json!({ "run": run, "grants": grants })),
                ));
            }

            "orchestration.cancel_run" => {
                let req: CancelRunRequest =
                    match parse_command_payload(&parsed, "orchestration.cancel_run") {
                        Ok(req) => req,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.cancel_run",
                                &err,
                            ));
                            continue;
                        }
                    };

                let mut run = if let Some(run) = load_runtime_snapshot(&runtime_store, &req.run_id)
                {
                    run
                } else {
                    let err =
                        Error::session(format!("orchestration run not found: {}", req.run_id));
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.cancel_run",
                        &err,
                    ));
                    continue;
                };
                let has_live_tasks = {
                    let rel = reliability_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
                    run_has_live_tasks(&rel, &run)
                };
                if has_live_tasks {
                    if let Err(err) = cancel_live_run_tasks_and_sync(
                        &cx,
                        &session,
                        &reliability_state,
                        &runtime_store,
                        &mut run,
                    )
                    .await
                    {
                        let _ = out_tx.send(response_error_with_hints(
                            id,
                            "orchestration.cancel_run",
                            &err,
                        ));
                        continue;
                    }
                } else {
                    run.phase = RunPhase::Canceled;
                    run.dispatch.active_wave = None;
                    run.dispatch.active_subrun_id = None;
                    run.touch();
                }
                if let Err(err) =
                    persist_runtime_snapshot(&cx, &session, &runtime_store, &run).await
                {
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.cancel_run",
                        &err,
                    ));
                    continue;
                }

                let _ = out_tx.send(response_ok(
                    id,
                    "orchestration.cancel_run",
                    Some(json!({ "run": run })),
                ));
            }

            "orchestration.resume_run" => {
                let req: RunLookupRequest =
                    match parse_command_payload(&parsed, "orchestration.resume_run") {
                        Ok(req) => req,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.resume_run",
                                &err,
                            ));
                            continue;
                        }
                    };

                let mut run = if let Some(run) = load_runtime_snapshot(&runtime_store, &req.run_id)
                {
                    run
                } else {
                    let err =
                        Error::session(format!("orchestration run not found: {}", req.run_id));
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.resume_run",
                        &err,
                    ));
                    continue;
                };
                if matches!(run.phase, RunPhase::Canceled | RunPhase::Failed) {
                    run.phase = RunPhase::Dispatching;
                    run.touch();
                }
                if let Some(status) = run
                    .dispatch
                    .latest_run_verify
                    .as_ref()
                    .filter(|status| !status.ok)
                {
                    let scope = completed_scope_from_run_verify(status);
                    let cwd = session_workspace_root(&cx, &session).await?;
                    execute_run_verification(&cwd, &mut run, &scope).await;
                }
                let has_live_tasks = {
                    let mut rel = reliability_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
                    refresh_live_run_from_reliability(&mut rel, &mut run)
                };
                if has_live_tasks
                    && !matches!(
                        run.phase,
                        RunPhase::Canceled
                            | RunPhase::Failed
                            | RunPhase::Completed
                            | RunPhase::AwaitingHuman
                    )
                {
                    let (updated_run, _) = match Box::pin(dispatch_run_until_quiescent(
                        &cx,
                        &session,
                        &reliability_state,
                        &runtime_store,
                        &options.config,
                        run,
                        "resume",
                        3600,
                    ))
                    .await
                    {
                        Ok(result) => result,
                        Err(err) => {
                            let _ = out_tx.send(response_error_with_hints(
                                id,
                                "orchestration.resume_run",
                                &err,
                            ));
                            continue;
                        }
                    };
                    run = updated_run;
                } else {
                    run.touch();
                }
                if let Err(err) =
                    persist_runtime_snapshot(&cx, &session, &runtime_store, &run).await
                {
                    let _ = out_tx.send(response_error_with_hints(
                        id,
                        "orchestration.resume_run",
                        &err,
                    ));
                    continue;
                }

                let _ = out_tx.send(response_ok(
                    id,
                    "orchestration.resume_run",
                    Some(json!({ "run": run })),
                ));
            }

            "get_session_stats" => {
                let data = {
                    let inner_session = session_handle.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    session_stats(&inner_session)
                };
                let _ = out_tx.send(response_ok(id, "get_session_stats", Some(data)));
            }

            "get_messages" => {
                let messages = {
                    let inner_session = session_handle.lock(&cx).await.map_err(|err| {
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
                };
                let messages = messages
                    .into_iter()
                    .map(rpc_session_message_value)
                    .collect::<Vec<_>>();
                let _ = out_tx.send(response_ok(
                    id,
                    "get_messages",
                    Some(json!({ "messages": messages })),
                ));
            }

            "get_available_models" => {
                let models = options
                    .available_models
                    .iter()
                    .map(rpc_model_from_entry)
                    .collect::<Vec<_>>();
                let _ = out_tx.send(response_ok(
                    id,
                    "get_available_models",
                    Some(json!({ "models": models })),
                ));
            }

            "set_model" => {
                let Some(provider) = parsed.get("provider").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_model",
                        "Missing provider".to_string(),
                    ));
                    continue;
                };
                let Some(model_id) = parsed.get("modelId").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_model",
                        "Missing modelId".to_string(),
                    ));
                    continue;
                };

                let Some(entry) = options
                    .available_models
                    .iter()
                    .find(|m| {
                        provider_ids_match(&m.model.provider, provider)
                            && m.model.id.eq_ignore_ascii_case(model_id)
                    })
                    .cloned()
                else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_model",
                        format!("Model not found: {provider}/{model_id}"),
                    ));
                    continue;
                };

                let key = resolve_model_key(&options.auth, &entry);
                if model_requires_configured_credential(&entry) && key.is_none() {
                    let err = Error::auth(format!(
                        "Missing credentials for {}/{}",
                        entry.model.provider, entry.model.id
                    ));
                    let _ = out_tx.send(response_error_with_hints(id, "set_model", &err));
                    continue;
                }

                {
                    let mut guard = session
                        .lock(&cx)
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

                let _ = out_tx.send(response_ok(
                    id,
                    "set_model",
                    Some(rpc_model_from_entry(&entry)),
                ));
            }

            "cycle_model" => {
                let (entry, thinking_level, is_scoped) = {
                    let mut guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let Some(result) = cycle_model_for_rpc(&mut guard, &options).await? else {
                        let _ =
                            out_tx.send(response_ok(id.clone(), "cycle_model", Some(Value::Null)));
                        continue;
                    };
                    result
                };

                let _ = out_tx.send(response_ok(
                    id,
                    "cycle_model",
                    Some(json!({
                        "model": rpc_model_from_entry(&entry),
                        "thinkingLevel": thinking_level.to_string(),
                        "isScoped": is_scoped,
                    })),
                ));
            }

            "set_thinking_level" => {
                let Some(level) = parsed.get("level").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_thinking_level",
                        "Missing level".to_string(),
                    ));
                    continue;
                };
                let level = match parse_thinking_level(level) {
                    Ok(level) => level,
                    Err(err) => {
                        let _ =
                            out_tx.send(response_error_with_hints(id, "set_thinking_level", &err));
                        continue;
                    }
                };

                {
                    let mut guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let level = {
                        let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        current_model_entry(&inner_session, &options)
                            .map_or(level, |entry| entry.clamp_thinking_level(level))
                    };
                    if let Err(err) = apply_thinking_level(&mut guard, level).await {
                        let _ = out_tx.send(response_error_with_hints(
                            id.clone(),
                            "set_thinking_level",
                            &err,
                        ));
                        continue;
                    }
                }
                let _ = out_tx.send(response_ok(id, "set_thinking_level", None));
            }

            "cycle_thinking_level" => {
                let next = {
                    let mut guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let entry = {
                        let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        current_model_entry(&inner_session, &options).cloned()
                    };
                    let Some(entry) = entry else {
                        let _ =
                            out_tx.send(response_ok(id, "cycle_thinking_level", Some(Value::Null)));
                        continue;
                    };
                    if !entry.model.reasoning {
                        let _ =
                            out_tx.send(response_ok(id, "cycle_thinking_level", Some(Value::Null)));
                        continue;
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
                let _ = out_tx.send(response_ok(
                    id,
                    "cycle_thinking_level",
                    Some(json!({ "level": next.to_string() })),
                ));
            }

            "set_steering_mode" => {
                let Some(mode) = parsed.get("mode").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_steering_mode",
                        "Missing mode".to_string(),
                    ));
                    continue;
                };
                let Some(mode) = parse_queue_mode(Some(mode)) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_steering_mode",
                        "Invalid steering mode".to_string(),
                    ));
                    continue;
                };
                let mut state = shared_state
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.steering_mode = mode;
                drop(state);
                sync_agent_queue_modes(&session, &shared_state, &cx).await?;
                let _ = out_tx.send(response_ok(id, "set_steering_mode", None));
            }

            "set_follow_up_mode" => {
                let Some(mode) = parsed.get("mode").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_follow_up_mode",
                        "Missing mode".to_string(),
                    ));
                    continue;
                };
                let Some(mode) = parse_queue_mode(Some(mode)) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_follow_up_mode",
                        "Invalid follow-up mode".to_string(),
                    ));
                    continue;
                };
                let mut state = shared_state
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.follow_up_mode = mode;
                drop(state);
                sync_agent_queue_modes(&session, &shared_state, &cx).await?;
                let _ = out_tx.send(response_ok(id, "set_follow_up_mode", None));
            }

            "set_auto_compaction" => {
                let Some(enabled) = parsed.get("enabled").and_then(Value::as_bool) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_auto_compaction",
                        "Missing enabled".to_string(),
                    ));
                    continue;
                };
                let mut state = shared_state
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.auto_compaction_enabled = enabled;
                drop(state);
                let _ = out_tx.send(response_ok(id, "set_auto_compaction", None));
            }

            "set_auto_retry" => {
                let Some(enabled) = parsed.get("enabled").and_then(Value::as_bool) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_auto_retry",
                        "Missing enabled".to_string(),
                    ));
                    continue;
                };
                let mut state = shared_state
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                state.auto_retry_enabled = enabled;
                drop(state);
                let _ = out_tx.send(response_ok(id, "set_auto_retry", None));
            }

            "abort_retry" => {
                retry_abort.store(true, Ordering::SeqCst);
                let _ = out_tx.send(response_ok(id, "abort_retry", None));
            }

            "set_session_name" => {
                let Some(name) = parsed.get("name").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "set_session_name",
                        "Missing name".to_string(),
                    ));
                    continue;
                };
                {
                    let mut guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    {
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        inner_session.append_session_info(Some(name.to_string()));
                    }
                    guard.persist_session().await?;
                }
                let _ = out_tx.send(response_ok(id, "set_session_name", None));
            }

            "get_last_assistant_text" => {
                let text = {
                    let inner_session = session_handle.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    last_assistant_text(&inner_session)
                };
                let _ = out_tx.send(response_ok(
                    id,
                    "get_last_assistant_text",
                    Some(json!({ "text": text })),
                ));
            }

            "export_html" => {
                let output_path = parsed
                    .get("outputPath")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // Capture a lightweight snapshot under lock, then release immediately.
                // This avoids cloning the full Session (caches, autosave queue, etc.)
                // and allows the HTML rendering + file I/O to proceed without holding
                // any session lock.
                let snapshot = {
                    let guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner.export_snapshot()
                };
                let path = export_html_snapshot(&snapshot, output_path.as_deref()).await?;
                let _ = out_tx.send(response_ok(
                    id,
                    "export_html",
                    Some(json!({ "path": path })),
                ));
            }

            "bash" => {
                let Some(command) = parsed.get("command").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(id, "bash", "Missing command".to_string()));
                    continue;
                };

                let mut running = bash_state
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?;
                if running.is_some() {
                    let _ = out_tx.send(response_error(
                        id,
                        "bash",
                        "Bash command already running".to_string(),
                    ));
                    continue;
                }

                let run_id = uuid::Uuid::new_v4().to_string();
                let (abort_tx, abort_rx) = oneshot::channel();
                *running = Some(RunningBash {
                    id: run_id.clone(),
                    abort_tx,
                });

                let out_tx = out_tx.clone();
                let session = Arc::clone(&session);
                let bash_state = Arc::clone(&bash_state);
                let command = command.to_string();
                let id_clone = id.clone();
                let runtime_handle = options.runtime_handle.clone();

                runtime_handle.spawn(async move {
                    let cx = AgentCx::for_request();
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let result = run_bash_rpc(&cwd, &command, abort_rx).await;

                    let response = match result {
                        Ok(result) => {
                            if let Ok(mut guard) = session.lock(&cx).await {
                                if let Ok(mut inner_session) = guard.session.lock(&cx).await {
                                    inner_session.append_message(SessionMessage::BashExecution {
                                        command: command.clone(),
                                        output: result.output.clone(),
                                        exit_code: result.exit_code,
                                        cancelled: Some(result.cancelled),
                                        truncated: Some(result.truncated),
                                        full_output_path: result.full_output_path.clone(),
                                        timestamp: Some(chrono::Utc::now().timestamp_millis()),
                                        extra: std::collections::HashMap::default(),
                                    });
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
            }

            "abort_bash" => {
                let mut running = bash_state
                    .lock(&cx)
                    .await
                    .map_err(|err| Error::session(format!("bash state lock failed: {err}")))?;
                if let Some(running_bash) = running.take() {
                    let _ = running_bash.abort_tx.send(&cx, ());
                }
                let _ = out_tx.send(response_ok(id, "abort_bash", None));
            }

            "compact" => {
                let custom_instructions = parsed
                    .get("customInstructions")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let data = {
                    let mut guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let path_entries = {
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
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
                        enabled: options.config.compaction_enabled(),
                        reserve_tokens: options.config.compaction_reserve_tokens(),
                        keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
                        ..Default::default()
                    };

                    let prep = prepare_compaction(&path_entries, settings).ok_or_else(|| {
                        Error::session(
                            "Compaction not available (already compacted or missing IDs)",
                        )
                    })?;

                    is_compacting.store(true, Ordering::SeqCst);
                    let result =
                        compact(prep, provider, key, custom_instructions.as_deref()).await?;
                    is_compacting.store(false, Ordering::SeqCst);
                    let details_value = compaction_details_to_value(&result.details)?;

                    let messages = {
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
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

                let _ = out_tx.send(response_ok(id, "compact", Some(data)));
            }

            "new_session" => {
                let parent = parsed
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                {
                    let mut guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let (session_dir, provider, model_id, thinking_level) = {
                        let inner_session = guard.session.lock(&cx).await.map_err(|err| {
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
                    // Keep model fields in header for clients.
                    new_session.header.provider.clone_from(&provider);
                    new_session.header.model_id.clone_from(&model_id);
                    new_session
                        .header
                        .thinking_level
                        .clone_from(&thinking_level);

                    let session_id = new_session.header.id.clone();
                    {
                        let mut inner_session = guard.session.lock(&cx).await.map_err(|err| {
                            Error::session(format!("inner session lock failed: {err}"))
                        })?;
                        *inner_session = new_session;
                    }
                    guard.agent.clear_messages();
                    guard.agent.stream_options_mut().session_id = Some(session_id);
                }
                {
                    let mut state = shared_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.steering.clear();
                    state.follow_up.clear();
                }
                let _ = out_tx.send(response_ok(
                    id,
                    "new_session",
                    Some(json!({ "cancelled": false })),
                ));
            }

            "switch_session" => {
                let Some(session_path) = parsed.get("sessionPath").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(
                        id,
                        "switch_session",
                        "Missing sessionPath".to_string(),
                    ));
                    continue;
                };

                let loaded = crate::session::Session::open(session_path).await;
                match loaded {
                    Ok(new_session) => {
                        let messages = new_session.to_messages_for_current_path();
                        let session_id = new_session.header.id.clone();
                        let mut guard = session
                            .lock(&cx)
                            .await
                            .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                        {
                            let mut inner_session =
                                guard.session.lock(&cx).await.map_err(|err| {
                                    Error::session(format!("inner session lock failed: {err}"))
                                })?;
                            *inner_session = new_session;
                        }
                        guard.agent.replace_messages(messages);
                        guard.agent.stream_options_mut().session_id = Some(session_id);
                        let _ = out_tx.send(response_ok(
                            id,
                            "switch_session",
                            Some(json!({ "cancelled": false })),
                        ));
                        let mut state = shared_state
                            .lock(&cx)
                            .await
                            .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                        state.steering.clear();
                        state.follow_up.clear();
                    }
                    Err(err) => {
                        let _ = out_tx.send(response_error_with_hints(id, "switch_session", &err));
                    }
                }
            }

            "fork" => {
                let Some(entry_id) = parsed.get("entryId").and_then(Value::as_str) else {
                    let _ = out_tx.send(response_error(id, "fork", "Missing entryId".to_string()));
                    continue;
                };

                // Phase 1: Snapshot — brief lock to compute ForkPlan + extract metadata.
                let (fork_plan, parent_path, session_dir, save_enabled, header_snapshot) = {
                    let guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    let plan = inner.plan_fork_from_user_message(entry_id)?;
                    let parent_path = inner.path.as_ref().map(|p| p.display().to_string());
                    let session_dir = inner.session_dir.clone();
                    let header = inner.header.clone();
                    (plan, parent_path, session_dir, guard.save_enabled(), header)
                    // Both locks released here.
                };

                // Phase 2: Build new session without holding any lock.
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

                // Phase 3: Swap — brief lock to install the new session.
                {
                    let mut guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let mut inner = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    *inner = new_session;
                    drop(inner);
                    guard.agent.replace_messages(messages);
                    guard.agent.stream_options_mut().session_id = Some(session_id);
                }

                {
                    let mut state = shared_state
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("state lock failed: {err}")))?;
                    state.steering.clear();
                    state.follow_up.clear();
                }

                let _ = out_tx.send(response_ok(
                    id,
                    "fork",
                    Some(json!({ "text": selected_text, "cancelled": false })),
                ));
            }

            "get_fork_messages" => {
                // Snapshot entries under brief lock, compute messages outside.
                let path_entries = {
                    let guard = session
                        .lock(&cx)
                        .await
                        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
                    let inner_session = guard.session.lock(&cx).await.map_err(|err| {
                        Error::session(format!("inner session lock failed: {err}"))
                    })?;
                    inner_session
                        .entries_for_current_path()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>()
                };
                let messages = fork_messages_from_entries(&path_entries);
                let _ = out_tx.send(response_ok(
                    id,
                    "get_fork_messages",
                    Some(json!({ "messages": messages })),
                ));
            }

            "get_commands" => {
                let commands = options.resources.list_commands();
                let _ = out_tx.send(response_ok(
                    id,
                    "get_commands",
                    Some(json!({ "commands": commands })),
                ));
            }

            "extension_ui_response" => {
                if let (Some(manager), Some(ui_state)) =
                    (rpc_extension_manager.as_ref(), rpc_ui_state.as_ref())
                {
                    let Some(request_id) = rpc_parse_extension_ui_response_id(&parsed) else {
                        let _ = out_tx.send(response_error(
                            id,
                            "extension_ui_response",
                            "Missing requestId (or id) field",
                        ));
                        continue;
                    };

                    let (response, next_request) = {
                        let Ok(mut guard) = ui_state.lock(&cx).await else {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                "Extension UI bridge unavailable",
                            ));
                            continue;
                        };

                        let Some(active) = guard.active.clone() else {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                "No active extension UI request",
                            ));
                            continue;
                        };

                        if active.id != request_id {
                            let _ = out_tx.send(response_error(
                                id,
                                "extension_ui_response",
                                format!(
                                    "Unexpected requestId: {request_id} (active: {})",
                                    active.id
                                ),
                            ));
                            continue;
                        }

                        let response = match rpc_parse_extension_ui_response(&parsed, &active) {
                            Ok(response) => response,
                            Err(message) => {
                                let _ = out_tx.send(response_error(
                                    id,
                                    "extension_ui_response",
                                    message,
                                ));
                                continue;
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
                    let _ = out_tx.send(response_ok(
                        id,
                        "extension_ui_response",
                        Some(json!({ "resolved": resolved })),
                    ));

                    if let Some(next) = next_request {
                        rpc_emit_extension_ui_request(
                            &options.runtime_handle,
                            Arc::clone(ui_state),
                            (*manager).clone(),
                            out_tx.clone(),
                            next,
                        );
                    }
                } else {
                    let _ = out_tx.send(response_ok(id, "extension_ui_response", None));
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

    // Explicitly shut down extension runtimes before the session drops.
    // Move the region out under lock, then await shutdown after releasing
    // the lock so we don't hold the session mutex across an async wait.
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

// =============================================================================
// Prompt Execution
// =============================================================================

#[allow(clippy::too_many_lines)]
async fn run_prompt_with_retry(
    session: Arc<Mutex<AgentSession>>,
    shared_state: Arc<Mutex<RpcSharedState>>,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    abort_handle_slot: Arc<Mutex<Option<AbortHandle>>>,
    out_tx: std::sync::mpsc::Sender<String>,
    retry_abort: Arc<AtomicBool>,
    options: RpcOptions,
    message: String,
    images: Vec<ImageContent>,
    cx: AgentCx,
) {
    retry_abort.store(false, Ordering::SeqCst);
    is_streaming.store(true, Ordering::SeqCst);

    let max_retries = options.config.retry_max_retries();
    let mut retry_count: u32 = 0;
    let mut success = false;
    let mut final_error: Option<String> = None;
    let mut final_error_hints: Option<Value> = None;

    loop {
        let (abort_handle, abort_signal) = AbortHandle::new();
        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
            *guard = Some(abort_handle);
        } else {
            is_streaming.store(false, Ordering::SeqCst);
            return;
        }

        let runtime_for_events = options.runtime_handle.clone();

        let result = {
            let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                Ok(guard) => guard,
                Err(err) => {
                    final_error = Some(format!("session lock failed: {err}"));
                    final_error_hints = None;
                    break;
                }
            };
            let extensions = guard.extensions.as_ref().map(|r| r.manager().clone());
            let runtime_for_events_handler = runtime_for_events.clone();
            let event_tx = out_tx.clone();
            let coalescer = extensions
                .as_ref()
                .map(|m| crate::extensions::EventCoalescer::new(m.clone()));
            let event_handler = move |event: AgentEvent| {
                let serialized = if let AgentEvent::AgentEnd {
                    messages, error, ..
                } = &event
                {
                    json!({
                        "type": "agent_end",
                        "messages": messages,
                        "error": error,
                    })
                    .to_string()
                } else {
                    serde_json::to_string(&event).unwrap_or_else(|err| {
                        json!({
                            "type": "event_serialize_error",
                            "error": err.to_string(),
                        })
                        .to_string()
                    })
                };
                let _ = event_tx.send(serialized);
                // Route non-lifecycle events through the coalescer for
                // batched/coalesced dispatch with lazy serialization.
                if let Some(coal) = &coalescer {
                    coal.dispatch_agent_event_lazy(&event, &runtime_for_events_handler);
                }
            };

            if images.is_empty() {
                guard
                    .run_text_with_abort(message.clone(), Some(abort_signal), event_handler)
                    .await
            } else {
                let mut blocks = vec![ContentBlock::Text(TextContent::new(message.clone()))];
                for image in &images {
                    blocks.push(ContentBlock::Image(image.clone()));
                }
                guard
                    .run_with_content_with_abort(blocks, Some(abort_signal), event_handler)
                    .await
            }
        };

        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&abort_handle_slot), &cx).await {
            *guard = None;
        }

        match result {
            Ok(message) => {
                if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                    final_error = message
                        .error_message
                        .clone()
                        .or_else(|| Some("Request error".to_string()));
                    final_error_hints = None;
                    if message.stop_reason == StopReason::Aborted {
                        break;
                    }
                    // Check if this error is retryable. Context overflow and
                    // auth failures should NOT be retried.
                    if let Some(ref err_msg) = final_error {
                        let context_window = if let Ok(guard) =
                            OwnedMutexGuard::lock(Arc::clone(&session), &cx).await
                        {
                            guard.session.lock(&cx).await.map_or(None, |inner| {
                                current_model_entry(&inner, &options)
                                    .map(|e| e.model.context_window)
                            })
                        } else {
                            None
                        };
                        if !crate::error::is_retryable_error(
                            err_msg,
                            Some(message.usage.input),
                            context_window,
                        ) {
                            break;
                        }
                    }
                } else {
                    success = true;
                    break;
                }
            }
            Err(err) => {
                let err_str = err.to_string();
                // No usage/context_window from an Err (no response received),
                // so pass None for both — text matching alone handles it.
                if !crate::error::is_retryable_error(&err_str, None, None) {
                    final_error = Some(err_str);
                    final_error_hints = Some(error_hints_value(&err));
                    break;
                }
                final_error = Some(err_str);
                final_error_hints = Some(error_hints_value(&err));
            }
        }

        let retry_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
            .await
            .is_ok_and(|state| state.auto_retry_enabled);
        if !retry_enabled || retry_count >= max_retries {
            break;
        }

        retry_count += 1;
        let delay_ms = retry_delay_ms(&options.config, retry_count);
        let error_message = final_error
            .clone()
            .unwrap_or_else(|| "Request error".to_string());
        let _ = out_tx.send(event(&json!({
            "type": "auto_retry_start",
            "attempt": retry_count,
            "maxAttempts": max_retries,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        })));

        let delay = Duration::from_millis(delay_ms as u64);
        let start = std::time::Instant::now();
        while start.elapsed() < delay {
            if retry_abort.load(Ordering::SeqCst) {
                break;
            }
            sleep(wall_now(), Duration::from_millis(50)).await;
        }

        if retry_abort.load(Ordering::SeqCst) {
            final_error = Some("Retry aborted".to_string());
            break;
        }
    }

    if retry_count > 0 {
        let _ = out_tx.send(event(&json!({
            "type": "auto_retry_end",
            "success": success,
            "attempt": retry_count,
            "finalError": if success { Value::Null } else { json!(final_error.clone()) },
        })));
    }

    is_streaming.store(false, Ordering::SeqCst);

    if !success {
        if let Some(err) = final_error {
            let mut payload = json!({
                "type": "agent_end",
                "messages": [],
                "error": err
            });
            if let Some(hints) = final_error_hints {
                payload["errorHints"] = hints;
            }
            let _ = out_tx.send(event(&payload));
        }
        return;
    }

    let auto_compaction_enabled = OwnedMutexGuard::lock(Arc::clone(&shared_state), &cx)
        .await
        .is_ok_and(|state| state.auto_compaction_enabled);
    if auto_compaction_enabled {
        maybe_auto_compact(session, options, is_compacting, out_tx).await;
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn response_ok(id: Option<String>, command: &str, data: Option<Value>) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": true,
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    if let Some(data) = data {
        resp["data"] = data;
    }
    resp.to_string()
}

fn response_error(id: Option<String>, command: &str, error: impl Into<String>) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": error.into(),
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    resp.to_string()
}

fn response_error_with_hints(id: Option<String>, command: &str, error: &Error) -> String {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": error.to_string(),
        "errorHints": error_hints_value(error),
    });
    if let Some(id) = id {
        resp["id"] = Value::String(id);
    }
    resp.to_string()
}

fn event(value: &Value) -> String {
    value.to_string()
}

fn rpc_emit_extension_ui_request(
    runtime_handle: &RuntimeHandle,
    ui_state: Arc<Mutex<RpcUiBridgeState>>,
    manager: ExtensionManager,
    out_tx_ui: std::sync::mpsc::Sender<String>,
    request: ExtensionUiRequest,
) {
    // Emit the UI request as a JSON notification to the client.
    let rpc_event = request.to_rpc_event();
    let _ = out_tx_ui.send(event(&rpc_event));

    if !request.expects_response() {
        return;
    }

    // For dialog methods, enforce deterministic ordering (one active request at a time) by
    // auto-resolving timeouts as cancellation defaults (per bd-2hz.1).
    let Some(timeout_ms) = request.effective_timeout_ms() else {
        return;
    };

    // Fire a little early so ExtensionManager::request_ui doesn't hit its own timeout first.
    let fire_ms = timeout_ms.saturating_sub(10).max(1);
    let request_id = request.id;
    let ui_state_timeout = Arc::clone(&ui_state);
    let manager_timeout = manager;
    let out_tx_timeout = out_tx_ui;
    let runtime_handle_inner = runtime_handle.clone();

    runtime_handle.spawn(async move {
        sleep(wall_now(), Duration::from_millis(fire_ms)).await;
        let cx = AgentCx::for_request();

        let next = {
            let Ok(mut guard) = ui_state_timeout.lock(cx.cx()).await else {
                return;
            };

            let Some(active) = guard.active.as_ref() else {
                return;
            };

            // No-op if the active request has already advanced.
            if active.id != request_id {
                return;
            }

            // Resolve with cancellation defaults (downstream maps method -> default return value).
            let _ = manager_timeout.respond_ui(ExtensionUiResponse {
                id: request_id,
                value: None,
                cancelled: true,
            });

            guard.active = None;
            let next = guard.queue.pop_front();
            if let Some(ref next) = next {
                guard.active = Some(next.clone());
            }
            next
        };

        if let Some(next) = next {
            rpc_emit_extension_ui_request(
                &runtime_handle_inner,
                ui_state_timeout,
                manager_timeout,
                out_tx_timeout,
                next,
            );
        }
    });
}

fn rpc_parse_extension_ui_response_id(parsed: &Value) -> Option<String> {
    let request_id = parsed
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);

    request_id.or_else(|| {
        parsed
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

fn rpc_parse_extension_ui_response(
    parsed: &Value,
    active: &ExtensionUiRequest,
) -> std::result::Result<ExtensionUiResponse, String> {
    let cancelled = parsed
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if cancelled {
        return Ok(ExtensionUiResponse {
            id: active.id.clone(),
            value: None,
            cancelled: true,
        });
    }

    match active.method.as_str() {
        "confirm" => {
            let value = parsed
                .get("confirmed")
                .and_then(Value::as_bool)
                .or_else(|| parsed.get("value").and_then(Value::as_bool))
                .ok_or_else(|| "confirm requires boolean `confirmed` (or `value`)".to_string())?;
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(Value::Bool(value)),
                cancelled: false,
            })
        }
        "select" => {
            let Some(value) = parsed.get("value") else {
                return Err("select requires `value` field".to_string());
            };

            let options = active
                .payload
                .get("options")
                .and_then(Value::as_array)
                .ok_or_else(|| "select request missing `options` array".to_string())?;

            let mut allowed = Vec::with_capacity(options.len());
            for opt in options {
                match opt {
                    Value::String(s) => allowed.push(Value::String(s.clone())),
                    Value::Object(map) => {
                        let label = map
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim();
                        if label.is_empty() {
                            continue;
                        }
                        if let Some(v) = map.get("value") {
                            allowed.push(v.clone());
                        } else {
                            allowed.push(Value::String(label.to_string()));
                        }
                    }
                    _ => {}
                }
            }

            if !allowed.iter().any(|candidate| candidate == value) {
                return Err("select response value did not match any option".to_string());
            }

            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(value.clone()),
                cancelled: false,
            })
        }
        "input" | "editor" => {
            let Some(value) = parsed.get("value") else {
                return Err(format!("{} requires `value` field", active.method));
            };
            if !value.is_string() {
                return Err(format!("{} requires string `value`", active.method));
            }
            Ok(ExtensionUiResponse {
                id: active.id.clone(),
                value: Some(value.clone()),
                cancelled: false,
            })
        }
        "notify" => Ok(ExtensionUiResponse {
            id: active.id.clone(),
            value: None,
            cancelled: false,
        }),
        other => Err(format!("Unsupported extension UI method: {other}")),
    }
}

#[cfg(test)]
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

fn error_hints_value(error: &Error) -> Value {
    let hint = error_hints::hints_for_error(error);
    json!({
        "summary": hint.summary,
        "hints": hint.hints,
        "contextFields": hint.context_fields,
    })
}

fn rpc_session_message_value(message: SessionMessage) -> Value {
    let mut value =
        serde_json::to_value(message).expect("SessionMessage should always serialize to JSON");
    rpc_flatten_content_blocks(&mut value);
    value
}

fn rpc_flatten_content_blocks(value: &mut Value) {
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

fn retry_delay_ms(config: &Config, attempt: u32) -> u32 {
    let base = u64::from(config.retry_base_delay_ms());
    let max = u64::from(config.retry_max_delay_ms());
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
}

#[cfg(test)]
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
                // First call fails with an explicit error event.
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
                // Second call succeeds.
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

fn should_auto_compact(tokens_before: u64, context_window: u32, reserve_tokens: u32) -> bool {
    let reserve = u64::from(reserve_tokens);
    let window = u64::from(context_window);
    tokens_before > window.saturating_sub(reserve)
}

#[allow(clippy::too_many_lines)]
async fn maybe_auto_compact(
    session: Arc<Mutex<AgentSession>>,
    options: RpcOptions,
    is_compacting: Arc<AtomicBool>,
    out_tx: std::sync::mpsc::Sender<String>,
) {
    let cx = AgentCx::for_request();
    let (path_entries, context_window, reserve_tokens, settings) = {
        let Ok(guard) = session.lock(cx.cx()).await else {
            return;
        };
        let (path_entries, context_window) = {
            let Ok(mut inner_session) = guard.session.lock(cx.cx()).await else {
                return;
            };
            inner_session.ensure_entry_ids();
            let Some(entry) = current_model_entry(&inner_session, &options) else {
                return;
            };
            let path_entries = inner_session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            (path_entries, entry.model.context_window)
        };

        let reserve_tokens = options.config.compaction_reserve_tokens();
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens,
            keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
            ..Default::default()
        };

        (path_entries, context_window, reserve_tokens, settings)
    };

    let Some(prep) = prepare_compaction(&path_entries, settings) else {
        return;
    };
    if !should_auto_compact(prep.tokens_before, context_window, reserve_tokens) {
        return;
    }

    let _ = out_tx.send(event(&json!({
        "type": "auto_compaction_start",
        "reason": "threshold",
    })));
    is_compacting.store(true, Ordering::SeqCst);

    let (provider, key) = {
        let Ok(guard) = session.lock(cx.cx()).await else {
            is_compacting.store(false, Ordering::SeqCst);
            return;
        };
        let Some(key) = guard.agent.stream_options().api_key.clone() else {
            is_compacting.store(false, Ordering::SeqCst);
            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": Value::Null,
                "aborted": false,
                "willRetry": false,
                "errorMessage": "Missing API key for compaction",
            })));
            return;
        };
        (guard.agent.provider(), key)
    };

    let result = compact(prep, provider, &key, None).await;
    is_compacting.store(false, Ordering::SeqCst);

    match result {
        Ok(result) => {
            let details_value = match compaction_details_to_value(&result.details) {
                Ok(value) => value,
                Err(err) => {
                    let _ = out_tx.send(event(&json!({
                        "type": "auto_compaction_end",
                        "result": Value::Null,
                        "aborted": false,
                        "willRetry": false,
                        "errorMessage": err.to_string(),
                    })));
                    return;
                }
            };

            let Ok(mut guard) = session.lock(cx.cx()).await else {
                return;
            };
            let messages = {
                let Ok(mut inner_session) = guard.session.lock(cx.cx()).await else {
                    return;
                };
                inner_session.append_compaction(
                    result.summary.clone(),
                    result.first_kept_entry_id.clone(),
                    result.tokens_before,
                    Some(details_value.clone()),
                    None,
                );
                inner_session.to_messages_for_current_path()
            };
            let _ = guard.persist_session().await;
            guard.agent.replace_messages(messages);
            drop(guard);

            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": {
                    "summary": result.summary,
                    "firstKeptEntryId": result.first_kept_entry_id,
                    "tokensBefore": result.tokens_before,
                    "details": details_value,
                },
                "aborted": false,
                "willRetry": false,
            })));
        }
        Err(err) => {
            let _ = out_tx.send(event(&json!({
                "type": "auto_compaction_end",
                "result": Value::Null,
                "aborted": false,
                "willRetry": false,
                "errorMessage": err.to_string(),
            })));
        }
    }
}

fn rpc_model_from_entry(entry: &ModelEntry) -> Value {
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

fn session_state(
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

fn session_stats(session: &crate::session::Session) -> Value {
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

fn last_assistant_text(session: &crate::session::Session) -> Option<String> {
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

/// Export HTML from a lightweight `ExportSnapshot` (non-blocking path).
///
/// The snapshot is captured under a brief lock, so the HTML rendering and
/// file I/O happen entirely outside any session lock.
async fn export_html_snapshot(
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

#[derive(Debug, Clone)]
struct BashRpcResult {
    output: String,
    exit_code: i32,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
}

const fn line_count_from_newline_count(
    total_bytes: usize,
    newline_count: usize,
    last_byte_was_newline: bool,
) -> usize {
    if total_bytes == 0 {
        0
    } else if last_byte_was_newline {
        newline_count
    } else {
        newline_count.saturating_add(1)
    }
}

async fn ingest_bash_rpc_chunk(
    bytes: Vec<u8>,
    chunks: &mut VecDeque<Vec<u8>>,
    chunks_bytes: &mut usize,
    total_bytes: &mut usize,
    total_lines: &mut usize,
    last_byte_was_newline: &mut bool,
    temp_file: &mut Option<asupersync::fs::File>,
    temp_file_path: &mut Option<PathBuf>,
    spill_failed: &mut bool,
    max_chunks_bytes: usize,
) {
    if bytes.is_empty() {
        return;
    }

    *last_byte_was_newline = bytes.last().is_some_and(|byte| *byte == b'\n');
    *total_bytes = total_bytes.saturating_add(bytes.len());
    *total_lines = total_lines.saturating_add(memchr_iter(b'\n', &bytes).count());

    // Spill to temp file if we exceed the limit
    if *total_bytes > DEFAULT_MAX_BYTES && temp_file.is_none() && !*spill_failed {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("pi-rpc-bash-{id}.log"));

        // Secure synchronous creation
        let created = {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path).is_ok()
        };

        if created {
            // Re-open async for writing
            match asupersync::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await
            {
                Ok(mut file) => {
                    // Flush existing chunks to the new file
                    for existing in chunks.iter() {
                        use asupersync::io::AsyncWriteExt;
                        if let Err(e) = file.write_all(existing).await {
                            tracing::warn!("Failed to flush bash chunk to temp file: {e}");
                            *spill_failed = true;
                            // Drop file to stop further writes
                            // Note: we can't easily drop 'mut file' here because we are in a loop
                            // but we can set temp_file to None below if we don't assign it.
                            break;
                        }
                    }
                    if !*spill_failed {
                        *temp_file = Some(file);
                        *temp_file_path = Some(path);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to reopen bash temp file async: {e}");
                    // Clean up the empty file we just created
                    let _ = std::fs::remove_file(&path);
                    *spill_failed = true;
                }
            }
        } else {
            *spill_failed = true;
        }
    }

    // Write new chunk to file if we have one
    if let Some(file) = temp_file.as_mut() {
        if *total_bytes <= crate::tools::BASH_FILE_LIMIT_BYTES {
            use asupersync::io::AsyncWriteExt;
            if let Err(e) = file.write_all(&bytes).await {
                tracing::warn!("Failed to write bash chunk to temp file: {e}");
                *spill_failed = true;
                *temp_file = None;
            }
        } else {
            // Hard limit reached. Stop writing and close the file to release the FD.
            if !*spill_failed {
                tracing::warn!("Bash output exceeded hard limit; stopping file log");
                *spill_failed = true;
                *temp_file = None;
            }
        }
    }

    // Update memory buffer
    *chunks_bytes = chunks_bytes.saturating_add(bytes.len());
    chunks.push_back(bytes);
    while *chunks_bytes > max_chunks_bytes && chunks.len() > 1 {
        if let Some(front) = chunks.pop_front() {
            *chunks_bytes = chunks_bytes.saturating_sub(front.len());
        }
    }
}

async fn run_bash_rpc(
    cwd: &std::path::Path,
    command: &str,
    abort_rx: oneshot::Receiver<()>,
) -> Result<BashRpcResult> {
    #[derive(Clone, Copy)]
    enum StreamKind {
        Stdout,
        Stderr,
    }

    struct StreamChunk {
        kind: StreamKind,
        bytes: Vec<u8>,
    }

    fn pump_stream(
        mut reader: impl std::io::Read,
        tx: std::sync::mpsc::SyncSender<StreamChunk>,
        kind: StreamKind,
    ) {
        let mut buf = [0u8; 8192];
        loop {
            let read = match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let chunk = StreamChunk {
                kind,
                bytes: buf[..read].to_vec(),
            };
            if tx.send(chunk).is_err() {
                break;
            }
        }
    }

    let shell = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("sh");

    let command = format!("trap 'code=$?; wait; exit $code' EXIT\n{command}");

    let mut child = std::process::Command::new(shell)
        .arg("-c")
        .arg(&command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell: {e}")))?;

    let Some(stdout) = child.stdout.take() else {
        return Err(Error::tool("bash", "Missing stdout".to_string()));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(Error::tool("bash", "Missing stderr".to_string()));
    };

    let mut guard = crate::tools::ProcessGuard::new(child, true);

    let (tx, rx) = std::sync::mpsc::sync_channel::<StreamChunk>(128);
    let tx_stdout = tx.clone();
    let stdout_handle =
        std::thread::spawn(move || pump_stream(stdout, tx_stdout, StreamKind::Stdout));
    let stderr_handle = std::thread::spawn(move || pump_stream(stderr, tx, StreamKind::Stderr));

    let tick = Duration::from_millis(10);

    // Bounded buffer state (same logic as BashTool)
    let mut chunks: VecDeque<Vec<u8>> = VecDeque::new();
    let mut chunks_bytes = 0usize;
    let mut total_bytes = 0usize;
    let mut total_lines = 0usize;
    let mut last_byte_was_newline = false;
    let mut temp_file: Option<asupersync::fs::File> = None;
    let mut temp_file_path: Option<PathBuf> = None;
    let max_chunks_bytes = DEFAULT_MAX_BYTES * 2;

    let mut cancelled = false;
    let mut spill_failed = false;

    let exit_code = loop {
        while let Ok(chunk) = rx.try_recv() {
            ingest_bash_rpc_chunk(
                chunk.bytes,
                &mut chunks,
                &mut chunks_bytes,
                &mut total_bytes,
                &mut total_lines,
                &mut last_byte_was_newline,
                &mut temp_file,
                &mut temp_file_path,
                &mut spill_failed,
                max_chunks_bytes,
            )
            .await;
        }

        if !cancelled && abort_rx.try_recv().is_ok() {
            cancelled = true;
            if let Ok(Some(status)) = guard.kill() {
                break status.code().unwrap_or(-1);
            }
        }

        match guard.try_wait_child() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(err) => {
                return Err(Error::tool(
                    "bash",
                    format!("Failed to wait for process: {err}"),
                ));
            }
        }

        sleep(wall_now(), tick).await;
    };

    // Drain remaining output
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    let mut drain_timed_out = false;
    loop {
        match rx.try_recv() {
            Ok(chunk) => {
                ingest_bash_rpc_chunk(
                    chunk.bytes,
                    &mut chunks,
                    &mut chunks_bytes,
                    &mut total_bytes,
                    &mut total_lines,
                    &mut last_byte_was_newline,
                    &mut temp_file,
                    &mut temp_file_path,
                    &mut spill_failed,
                    max_chunks_bytes,
                )
                .await;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if Instant::now() >= drain_deadline {
                    drain_timed_out = true;
                    break;
                }
                sleep(wall_now(), tick).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    // Drop the receiver to close the channel.
    // This ensures that any `tx.send()` calls in the pump threads return an error (Disconnected)
    // instead of blocking if the channel is full, preventing a deadlock on `join()`.
    drop(rx);

    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Explicitly drop the temp file handle to ensure any buffered data is flushed to disk
    // before we potentially return the path to the caller.
    drop(temp_file);

    // Construct final output from memory buffer
    let mut combined = Vec::with_capacity(chunks_bytes);
    for chunk in chunks {
        combined.extend_from_slice(&chunk);
    }
    let tail_output = String::from_utf8_lossy(&combined).to_string();

    let mut truncation = truncate_tail(tail_output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if total_bytes > chunks_bytes {
        truncation.truncated = true;
        truncation.truncated_by = Some(crate::tools::TruncatedBy::Bytes);
        truncation.total_bytes = total_bytes;
        truncation.total_lines =
            line_count_from_newline_count(total_bytes, total_lines, last_byte_was_newline);
    } else if drain_timed_out {
        truncation.truncated = true;
        truncation.truncated_by = Some(crate::tools::TruncatedBy::Bytes);
    }
    let will_truncate = truncation.truncated;

    let mut output_text = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content
    };

    if drain_timed_out {
        output_text.push_str("\n... [Output truncated: drain timeout]");
    }

    Ok(BashRpcResult {
        output: output_text,
        exit_code,
        cancelled,
        truncated: will_truncate,
        full_output_path: temp_file_path.map(|p| p.display().to_string()),
    })
}

fn parse_prompt_images(value: Option<&Value>) -> Result<Vec<ImageContent>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(arr) = value.as_array() else {
        return Err(Error::validation("images must be an array"));
    };

    let mut images = Vec::new();
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type != "image" {
            continue;
        }
        let Some(source) = obj.get("source").and_then(Value::as_object) else {
            continue;
        };
        let source_type = source.get("type").and_then(Value::as_str).unwrap_or("");
        if source_type != "base64" {
            continue;
        }
        let Some(media_type) = source.get("mediaType").and_then(Value::as_str) else {
            continue;
        };
        let Some(data) = source.get("data").and_then(Value::as_str) else {
            continue;
        };
        images.push(ImageContent {
            data: data.to_string(),
            mime_type: media_type.to_string(),
        });
    }
    Ok(images)
}

fn resolve_model_key(auth: &AuthStorage, entry: &ModelEntry) -> Option<String> {
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

fn model_requires_configured_credential(entry: &ModelEntry) -> bool {
    let provider = entry.model.provider.as_str();
    entry.auth_header
        || provider_metadata(provider).is_some_and(|meta| !meta.auth_env_keys.is_empty())
        || entry.oauth_config.is_some()
}

fn parse_thinking_level(level: &str) -> Result<crate::model::ThinkingLevel> {
    level.parse().map_err(|err: String| Error::validation(err))
}

fn current_model_entry<'a>(
    session: &crate::session::Session,
    options: &'a RpcOptions,
) -> Option<&'a ModelEntry> {
    let provider = session.header.provider.as_deref()?;
    let model_id = session.header.model_id.as_deref()?;
    options.available_models.iter().find(|m| {
        provider_ids_match(&m.model.provider, provider) && m.model.id.eq_ignore_ascii_case(model_id)
    })
}

async fn apply_thinking_level(
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

async fn apply_model_change(guard: &mut AgentSession, entry: &ModelEntry) -> Result<()> {
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

/// Extract user messages from a pre-captured list of session entries.
///
/// Used by the non-blocking `get_fork_messages` path where entries are
/// captured under a brief lock and messages are computed outside the lock.
fn fork_messages_from_entries(entries: &[crate::session::SessionEntry]) -> Vec<Value> {
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

fn extract_user_text(content: &crate::model::UserContent) -> Option<String> {
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

/// Returns the available thinking levels for a model.
/// For reasoning models, returns the full range; for non-reasoning, returns only Off.
fn available_thinking_levels(entry: &ModelEntry) -> Vec<crate::model::ThinkingLevel> {
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

/// Cycles through scoped models (if any) and returns the next model.
/// Returns (ModelEntry, ThinkingLevel, is_from_scoped_models).
async fn cycle_model_for_rpc(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentSession};
    use crate::auth::AuthCredential;
    use crate::compaction::ResolvedCompactionSettings;
    use crate::config::ReliabilityConfig;
    use crate::model::{
        AssistantMessage, ContentBlock, ImageContent, StopReason, StreamEvent, TextContent,
        ThinkingLevel, Usage, UserContent, UserMessage,
    };
    use crate::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
    use crate::reliability::ClosePayload;
    use crate::session::Session;
    use crate::tools::ToolRegistry;
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

    fn test_task_node(task_id: &str) -> TaskNode {
        TaskNode::new(TaskSpec {
            task_id: task_id.to_string(),
            title: format!("Task {task_id}"),
            objective: format!("Objective {task_id}"),
            parent_goal_trace_id: Some(format!("goal:{task_id}")),
            planned_touches: Vec::new(),
            input_snapshot: Some("snapshot".to_string()),
            max_attempts: 3,
            enforce_symbol_drift_check: false,
            verify: VerifySpec {
                command: "true".to_string(),
                timeout_sec: 30,
                acceptance_ids: Vec::new(),
            },
            autonomy: AutonomyLevel::Guarded,
            constraints: TaskConstraints::default(),
        })
    }

    fn test_run_snapshot(
        run_id: &str,
        objective: &str,
        selected_tier: ExecutionTier,
    ) -> RunSnapshot {
        let mut run = RunSnapshot::new(RunSpec {
            run_id: run_id.to_string(),
            objective: objective.to_string(),
            root_workspace: PathBuf::from("/tmp/pi-agent-rust-tests"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            run_verify_command: None,
            run_verify_timeout_sec: None,
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        });
        run.dispatch.selected_tier = selected_tier;
        run.phase = RunPhase::Dispatching;
        run.touch();
        run
    }

    fn set_run_task_ids(run: &mut RunSnapshot, task_ids: Vec<String>) {
        run.tasks = task_ids
            .into_iter()
            .map(|task_id| (task_id.clone(), test_task_node(&task_id)))
            .collect();
        run.touch();
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
            let mut contents =
                fs::read_to_string(&target).map_err(|err| Error::Io(Box::new(err)))?;

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

        let mut primary =
            reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
        primary.set_external_lease_provider(provider.clone());

        let mut secondary =
            reliability_state_for_tests(ReliabilityEnforcementMode::Hard, false, true);
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
    fn orchestration_run_id_availability_rejects_cached_and_persisted_runs() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let persisted_snapshot = RunSnapshot::new(RunSpec {
            run_id: "run-persisted".to_string(),
            objective: "Persisted".to_string(),
            root_workspace: PathBuf::from("/tmp/persisted"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            run_verify_command: Some("cargo test".to_string()),
            run_verify_timeout_sec: Some(60),
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        });
        runtime_store
            .save_snapshot(&persisted_snapshot)
            .expect("save persisted snapshot");

        let persisted_err = ensure_run_id_available(&runtime_store, "run-persisted")
            .expect_err("persisted run should conflict");
        assert!(persisted_err.to_string().contains("already exists"));

        let runtime_snapshot = RunSnapshot::new(RunSpec {
            run_id: "run-runtime".to_string(),
            objective: "Runtime".to_string(),
            root_workspace: PathBuf::from("/tmp/runtime"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            run_verify_command: Some("cargo test".to_string()),
            run_verify_timeout_sec: Some(60),
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        });
        runtime_store
            .save_snapshot(&runtime_snapshot)
            .expect("save runtime snapshot");

        let runtime_err = ensure_run_id_available(&runtime_store, "run-runtime")
            .expect_err("runtime run should conflict");
        assert!(runtime_err.to_string().contains("already exists"));

        assert!(ensure_run_id_available(&runtime_store, "run-new").is_ok());
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

        let mut run = test_run_snapshot("run-wave", "Wave", ExecutionTier::Wave);
        set_run_task_ids(
            &mut run,
            vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
        );
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Running);
        assert_eq!(run.summary.task_counts.get("leased"), Some(&1));
        assert_eq!(run.summary.task_counts.get("ready"), Some(&1));
        assert_eq!(
            run.dispatch
                .active_wave
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

        let mut run = test_run_snapshot("run-ready", "Ready wave", ExecutionTier::Wave);
        set_run_task_ids(
            &mut run,
            vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
        );
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Dispatching);
        assert_eq!(
            run.dispatch
                .active_wave
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

        let mut run = test_run_snapshot("run-recoverable", "Recoverable wait", ExecutionTier::Wave);
        set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Recovering);
        assert_eq!(run.summary.task_counts.get("recoverable"), Some(&1));
        assert!(run.wake_at.is_some());
        assert!(run.dispatch.active_wave.is_none());
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

        let mut run = test_run_snapshot("run-mixed", "Mixed run", ExecutionTier::Wave);
        set_run_task_ids(
            &mut run,
            vec![
                contract_running.task_id.clone(),
                contract_recoverable.task_id.clone(),
            ],
        );
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Running);
        assert_eq!(
            run.dispatch
                .active_wave
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

        let mut run = test_run_snapshot("run-dispatch", "Dispatch wave", ExecutionTier::Wave);
        run.spec.budgets.max_parallelism = 2;
        set_run_task_ids(
            &mut run,
            vec![
                contract_a.task_id.clone(),
                contract_b.task_id.clone(),
                contract_c.task_id.clone(),
            ],
        );

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
        assert_eq!(run.phase, RunPhase::Running);
        assert_eq!(run.summary.task_counts.get("leased"), Some(&2));
        assert_eq!(run.summary.task_counts.get("ready"), Some(&1));
        assert_eq!(
            run.dispatch
                .active_wave
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

        let mut run = test_run_snapshot("run-dispatch", "Dispatch wave", ExecutionTier::Wave);
        run.spec.budgets.max_parallelism = 2;
        set_run_task_ids(
            &mut run,
            vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
        );

        let first_grants =
            dispatch_run_wave(&mut reliability, &mut run, "worker", 120).expect("first dispatch");
        let second_grants = dispatch_run_wave(&mut reliability, &mut run, "worker", 120)
            .expect("repeat dispatch should be idempotent");

        assert_eq!(first_grants.len(), 2);
        assert!(second_grants.is_empty());
        assert_eq!(run.phase, RunPhase::Running);
        assert_eq!(run.summary.task_counts.get("leased"), Some(&2));
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

        let mut run = test_run_snapshot("run-cancel", "Cancel run", ExecutionTier::Wave);
        run.spec.budgets.max_parallelism = 2;
        set_run_task_ids(
            &mut run,
            vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
        );

        let grants =
            dispatch_run_wave(&mut reliability, &mut run, "worker", 120).expect("dispatch run");

        assert_eq!(grants.len(), 2);
        assert_eq!(run.phase, RunPhase::Running);
        assert!(run.dispatch.active_wave.is_some());

        let canceled_grants = cancel_live_run_tasks(&mut reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Canceled);
        assert!(run.dispatch.active_wave.is_none());
        assert!(run.dispatch.active_subrun_id.is_none());
        assert_eq!(run.summary.task_counts.get("ready"), Some(&2));
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            false,
            true,
        )));

        run_async(async {
            let mut run = test_run_snapshot(
                "run-cancel-sync",
                "Cancel run sync fixture",
                ExecutionTier::Wave,
            );
            run.spec.budgets.max_parallelism = 2;
            set_run_task_ids(&mut run, vec!["task-a".to_string(), "task-b".to_string()]);

            let grants = {
                let mut rel = reliability_state.lock(&cx).await.expect("lock reliability");
                rel.get_or_create_task(&reliability_contract("task-a", Vec::new()))
                    .expect("register task a");
                rel.get_or_create_task(&reliability_contract("task-b", Vec::new()))
                    .expect("register task b");
                dispatch_run_wave(&mut rel, &mut run, "worker", 120).expect("dispatch run")
            };

            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
                .await
                .expect("persist run");
            append_dispatch_grants_session_entries(&cx, &session, &reliability_state, &grants)
                .await
                .expect("append dispatch entries");

            cancel_live_run_tasks_and_sync(
                &cx,
                &session,
                &reliability_state,
                &runtime_store,
                &mut run,
            )
            .await
            .expect("cancel live run");

            assert_eq!(run.phase, RunPhase::Canceled);
            assert_eq!(run.dispatch.task_reports.len(), 2);
            assert!(run.dispatch.active_wave.is_none());
            assert!(
                run.dispatch
                    .task_reports
                    .values()
                    .all(|report| report.summary.contains("canceled dispatch"))
            );

            let persisted = runtime_store
                .load_snapshot(&run.spec.run_id)
                .expect("load persisted run");
            assert_eq!(persisted.phase, RunPhase::Canceled);
            assert_eq!(persisted.dispatch.task_reports.len(), 2);

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

        let mut run = test_run_snapshot(
            "run-canceled-refresh",
            "Canceled refresh fixture",
            ExecutionTier::Wave,
        );
        set_run_task_ids(&mut run, vec!["task-a".to_string(), "task-b".to_string()]);
        run.phase = RunPhase::Canceled;
        run.touch();
        run.dispatch.active_wave = Some(WaveStatus {
            wave_id: "wave-stale".to_string(),
            task_ids: vec!["task-a".to_string()],
            started_at: chrono::Utc::now(),
            completed_at: None,
        });
        run.dispatch.active_subrun_id = Some("subrun-stale".to_string());

        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Canceled);
        assert!(run.dispatch.active_wave.is_none());
        assert!(run.dispatch.active_subrun_id.is_none());
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

        let mut run = test_run_snapshot("run-serialized", "Serialized wave", ExecutionTier::Wave);
        set_run_task_ids(
            &mut run,
            vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
        );
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Dispatching);
        assert_eq!(
            run.dispatch
                .active_wave
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

        let mut run = test_run_snapshot("run-cap", "Capped wave", ExecutionTier::Wave);
        run.spec.budgets.max_parallelism = 2;
        set_run_task_ids(&mut run, task_ids);
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.phase, RunPhase::Dispatching);
        assert_eq!(run.effective_max_parallelism(), 2);
        assert_eq!(
            run.dispatch
                .active_wave
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

        let mut run = test_run_snapshot("run-hier", "Hierarchical", ExecutionTier::Hierarchical);
        run.spec.budgets.max_parallelism = 12;
        set_run_task_ids(&mut run, task_ids.clone());
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.dispatch.active_subrun_id.as_deref(), Some("subrun-01"));
        assert_eq!(run.effective_max_parallelism(), 12);
        assert_eq!(run.dispatch.planned_subruns.len(), 2);
        assert_eq!(run.dispatch.planned_subruns[0].task_ids.len(), 12);
        assert_eq!(run.dispatch.planned_subruns[1].task_ids.len(), 1);
        assert_eq!(
            run.dispatch
                .active_wave
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

        let mut run = test_run_snapshot(
            "run-hier-chain",
            "Hierarchical chain",
            ExecutionTier::Hierarchical,
        );
        set_run_task_ids(&mut run, task_ids.clone());
        refresh_run_from_reliability(&reliability, &mut run);

        assert_eq!(run.dispatch.planned_subruns.len(), 2);
        assert_eq!(run.dispatch.active_subrun_id.as_deref(), Some("subrun-01"));
        assert_eq!(
            run.dispatch
                .active_wave
                .as_ref()
                .map(|wave| wave.task_ids.clone())
                .unwrap_or_default(),
            vec!["task-chain-00".to_string()]
        );
    }

    #[test]
    fn orchestration_submit_rollup_marks_success_and_captures_task_report() {
        let mut reliability =
            reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
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

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let mut run = test_run_snapshot("run-success", "Run success", ExecutionTier::Inline);
        set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
        runtime_store
            .save_snapshot(&run)
            .expect("save run snapshot");

        let updated = refresh_task_runs(
            &reliability,
            &runtime_store,
            &contract.task_id,
            Some(report),
        );
        let run = updated.first().expect("updated run");
        let task_report = run
            .dispatch
            .task_reports
            .get(&contract.task_id)
            .expect("persisted task report");

        assert_eq!(run.phase, RunPhase::Completed);
        assert_eq!(task_report.summary, "Implemented orchestration report");
        assert_eq!(task_report.verify_exit_code, 0);
        assert_eq!(task_report.changed_files, vec!["src/rpc.rs".to_string()]);
    }

    #[test]
    fn orchestration_inline_execution_substrate_applies_verified_diff_and_closes_task() {
        let (temp_dir, repo_path, head) = setup_inline_execution_repo();
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-inline-exec",
                "Inline execution fixture",
                ExecutionTier::Inline,
            );
            set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
            run.spec.run_verify_command = Some("true".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
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
                &runtime_store,
                &repo_path,
                "run-inline-exec",
                &grant,
                &worker,
            )
            .await
            .expect("execute inline task");

            assert_eq!(updated_run.phase, RunPhase::Completed);
            assert!(
                updated_run
                    .dispatch
                    .latest_run_verify
                    .as_ref()
                    .is_some_and(|verify| verify.ok)
            );
            assert_eq!(
                updated_run
                    .dispatch
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-inline-fail",
                "Inline execution verify failure",
                ExecutionTier::Inline,
            );
            set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
            run.spec.run_verify_command = Some("true".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
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
                &runtime_store,
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-inline-worker-fail",
                "Inline execution worker failure",
                ExecutionTier::Inline,
            );
            set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
            run.spec.run_verify_command = Some("true".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
                .await
                .expect("persist initial run");

            let worker = FailingInlineWorker {
                message: "fixture worker exploded".to_string(),
            };
            let err = execute_inline_dispatch_grant_with_worker(
                &cx,
                &session,
                &reliability_state,
                &runtime_store,
                &repo_path,
                "run-inline-worker-fail",
                &grant,
                &worker,
            )
            .await
            .expect_err("worker failure should surface");
            assert!(err.to_string().contains("fixture worker exploded"));

            let run = current_run_snapshot(&cx, &runtime_store, "run-inline-worker-fail")
                .await
                .expect("load updated run");
            let report = run
                .dispatch
                .task_reports
                .get(&contract.task_id)
                .expect("rollback task report");

            assert_eq!(run.phase, RunPhase::Recovering);
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-inline-create",
                "Inline execution create-file fixture",
                ExecutionTier::Inline,
            );
            set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
            run.spec.run_verify_command = Some("test -f src/new_file.rs".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
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
                &runtime_store,
                &repo_path,
                "run-inline-create",
                &grant,
                &worker,
            )
            .await
            .expect("execute inline create task");

            assert_eq!(updated_run.phase, RunPhase::Completed);
            assert!(
                updated_run
                    .dispatch
                    .latest_run_verify
                    .as_ref()
                    .is_some_and(|verify| verify.ok)
            );
            assert_eq!(
                updated_run
                    .dispatch
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-parallel-wave",
                "Parallel wave execution fixture",
                ExecutionTier::Wave,
            );
            set_run_task_ids(
                &mut run,
                vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
            );
            run.spec.run_verify_command = Some(
                "grep -q task_parallel_a src/lib.rs && grep -q task_parallel_b src/extra.rs"
                    .to_string(),
            );
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
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
                &runtime_store,
                &repo_path,
                run,
                &grants,
                &worker,
            )
            .await
            .expect("execute parallel wave");

            assert_eq!(updated_run.phase, RunPhase::Completed);
            assert!(
                updated_run
                    .dispatch
                    .latest_run_verify
                    .as_ref()
                    .is_some_and(|verify| verify.ok)
            );
            assert_eq!(max_in_flight.load(Ordering::SeqCst), 2);
            let lib_contents =
                fs::read_to_string(repo_path.join("src/lib.rs")).expect("read updated lib file");
            let extra_contents = fs::read_to_string(repo_path.join("src/extra.rs"))
                .expect("read updated extra file");
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-partial-wave",
                "Parallel partial failure fixture",
                ExecutionTier::Wave,
            );
            set_run_task_ids(
                &mut run,
                vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
            );
            run.spec.run_verify_command = Some("true".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
                .await
                .expect("persist initial run");

            let worker = PartialWaveFailureWorker {
                failing_task_id: "task-partial-b".to_string(),
            };
            let updated_run = execute_dispatch_grants_with_worker(
                &cx,
                &session,
                &reliability_state,
                &runtime_store,
                &repo_path,
                run,
                &grants,
                &worker,
            )
            .await
            .expect("partial wave failure should stay automatable");
            assert_eq!(updated_run.phase, RunPhase::Recovering);

            let lib_contents =
                fs::read_to_string(repo_path.join("src/lib.rs")).expect("read updated lib file");
            let extra_contents =
                fs::read_to_string(repo_path.join("src/extra.rs")).expect("read extra file");
            assert!(lib_contents.contains("task_partial_a"));
            assert!(!extra_contents.contains("task_partial_b"));

            let updated_run = current_run_snapshot(&cx, &runtime_store, "run-partial-wave")
                .await
                .expect("load updated run");
            assert_eq!(updated_run.phase, RunPhase::Recovering);
            assert_eq!(
                updated_run
                    .dispatch
                    .task_reports
                    .get("task-partial-a")
                    .and_then(|report| report.failure_class.clone()),
                None
            );
            assert_eq!(
                updated_run
                    .dispatch
                    .task_reports
                    .get("task-partial-b")
                    .and_then(|report| report.failure_class.as_deref()),
                Some("orchestration_execution_error")
            );
            assert!(
                updated_run
                    .dispatch
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-overlap-replay",
                "Overlap replay fixture",
                ExecutionTier::Wave,
            );
            set_run_task_ids(
                &mut run,
                vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
            );
            run.spec.run_verify_command = Some(
                "grep -q \"beta-after-alpha\" src/lib.rs && ! grep -q \"beta-alone\" src/lib.rs"
                    .to_string(),
            );
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
                .await
                .expect("persist initial run");

            let worker = OverlapReplayFixtureWorker {
                target: "src/lib.rs".to_string(),
            };
            let updated_run = execute_dispatch_grants_with_worker(
                &cx,
                &session,
                &reliability_state,
                &runtime_store,
                &repo_path,
                run,
                &grants,
                &worker,
            )
            .await
            .expect("execute overlap replay");

            assert_eq!(updated_run.phase, RunPhase::Completed);
            assert!(
                updated_run
                    .dispatch
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

            let mut run = test_run_snapshot(
                "run-sequential-rebase",
                "Sequential rebase fixture",
                ExecutionTier::Wave,
            );
            set_run_task_ids(
                &mut run,
                vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
            );
            run.spec.run_verify_command = Some("true".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            {
                let rel = reliability_state.lock(&cx).await.expect("lock reliability");
                refresh_run_from_reliability(&rel, &mut run);
            }
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
                .await
                .expect("persist initial run");

            let worker = OverlapReplayFixtureWorker {
                target: "src/lib.rs".to_string(),
            };
            let after_first = execute_inline_dispatch_grant_with_worker(
                &cx,
                &session,
                &reliability_state,
                &runtime_store,
                &repo_path,
                "run-sequential-rebase",
                &grant_a,
                &worker,
            )
            .await
            .expect("execute first sequential task");
            assert!(matches!(
                after_first.phase,
                RunPhase::Dispatching | RunPhase::Running
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
                &runtime_store,
                &repo_path,
                "run-sequential-rebase",
                &grant_b,
                &worker,
            )
            .await
            .expect("execute second sequential task");

            assert_eq!(updated_run.phase, RunPhase::Completed);
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
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let cx = AgentCx::for_request();
        let session = build_test_agent_session(&repo_path);
        let reliability_state = Arc::new(Mutex::new(reliability_state_for_tests(
            ReliabilityEnforcementMode::Hard,
            true,
            true,
        )));

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

                let mut run = test_run_snapshot(
                    "run-chain-wave",
                    "Same-touch prerequisite chain",
                    ExecutionTier::Wave,
                );
                set_run_task_ids(
                    &mut run,
                    vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
                );
                refresh_run_from_reliability(&rel, &mut run);
                run
            };
            persist_runtime_snapshot(&cx, &session, &runtime_store, &run)
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
                &runtime_store,
                &repo_path,
                "run-chain-wave",
                &first_grant,
                &worker,
            )
            .await
            .expect("execute first wave");

            let mut run = current_run_snapshot(&cx, &runtime_store, "run-chain-wave")
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

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let runtime_store = RuntimeStore::new(temp_dir.path().join("runtime"));
        let mut run = test_run_snapshot("run-blocked", "Run blocked", ExecutionTier::Inline);
        set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
        runtime_store
            .save_snapshot(&run)
            .expect("save run snapshot");

        let updated = refresh_task_runs(
            &reliability,
            &runtime_store,
            &contract.task_id,
            Some(report),
        );
        let run = updated.first().expect("updated run");
        let task_report = run
            .dispatch
            .task_reports
            .get(&contract.task_id)
            .expect("persisted task report");

        assert_eq!(run.phase, RunPhase::AwaitingHuman);
        assert_eq!(task_report.failure_class.as_deref(), Some("human_blocker"));
        assert!(
            task_report
                .blockers
                .iter()
                .any(|blocker| blocker.contains("Need approval"))
        );
    }

    #[test]
    fn orchestration_completed_run_verify_scope_detects_run_completion() {
        let mut reliability =
            reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
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

        let mut run = test_run_snapshot("run-wave-verify", "Run verify", ExecutionTier::Wave);
        run.spec.run_verify_command = Some("true".to_string());
        run.spec.run_verify_timeout_sec = Some(30);
        set_run_task_ids(
            &mut run,
            vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
        );
        run.dispatch.active_wave = Some(WaveStatus {
            wave_id: "wave-1".to_string(),
            task_ids: run.task_ids(),
            started_at: chrono::Utc::now(),
            completed_at: None,
        });

        let scope = completed_run_verify_scope(&reliability, &run).expect("completed run scope");
        assert_eq!(scope.scope_id, run.spec.run_id);
        assert_eq!(scope.scope_kind, RunVerifyScopeKind::Run);
        assert_eq!(scope.subrun_id, None);
    }

    #[test]
    fn orchestration_completed_run_verify_scope_waits_for_full_run_completion() {
        let mut reliability =
            reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
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

        let mut run = test_run_snapshot(
            "run-wave-intermediate",
            "Run verify waits for terminal run",
            ExecutionTier::Wave,
        );
        run.spec.run_verify_command = Some("true".to_string());
        run.spec.run_verify_timeout_sec = Some(30);
        set_run_task_ids(
            &mut run,
            vec![contract_a.task_id.clone(), contract_b.task_id.clone()],
        );
        run.dispatch.active_wave = Some(WaveStatus {
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
        let mut reliability =
            reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
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

        let mut run = test_run_snapshot(
            "run-hier-intermediate",
            "Run verify waits for terminal hierarchical run",
            ExecutionTier::Hierarchical,
        );
        run.spec.run_verify_command = Some("true".to_string());
        run.spec.run_verify_timeout_sec = Some(30);
        set_run_task_ids(&mut run, task_ids.clone());
        refresh_run_from_reliability(&reliability, &mut run);
        assert_eq!(run.dispatch.planned_subruns.len(), 2);

        let first_subrun = run
            .dispatch
            .planned_subruns
            .first()
            .cloned()
            .expect("first subrun");
        assert_eq!(first_subrun.task_ids.len(), 12);
        run.dispatch.active_subrun_id = Some(first_subrun.subrun_id.clone());
        run.dispatch.active_wave = Some(WaveStatus {
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
        let mut reliability =
            reliability_state_for_tests(ReliabilityEnforcementMode::Hard, true, true);
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

        let mut run = test_run_snapshot(
            "run-verify-fail-state",
            "Run verify failed",
            ExecutionTier::Inline,
        );
        set_run_task_ids(&mut run, vec![contract.task_id.clone()]);
        run.dispatch.latest_run_verify = Some(RunVerifyStatus {
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
        assert_eq!(run.phase, RunPhase::Failed);
    }

    #[test]
    fn orchestration_execute_run_verification_records_success() {
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .blocking_threads(1, 8)
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let mut run = test_run_snapshot(
                "run-verify-success",
                "Run verify success",
                ExecutionTier::Wave,
            );
            run.spec.run_verify_command = Some("true".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            let scope = CompletedRunVerifyScope {
                scope_id: "run-verify-success".to_string(),
                scope_kind: RunVerifyScopeKind::Run,
                subrun_id: None,
            };

            execute_run_verification(temp_dir.path(), &mut run, &scope).await;

            let verify = run
                .dispatch
                .latest_run_verify
                .as_ref()
                .expect("verification result");
            assert!(verify.ok);
            assert_eq!(verify.scope_id, "run-verify-success");
            assert_eq!(run.phase, RunPhase::Dispatching);
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
            let mut run = test_run_snapshot(
                "run-verify-failure",
                "Run verify failure",
                ExecutionTier::Wave,
            );
            run.spec.run_verify_command = Some("false".to_string());
            run.spec.run_verify_timeout_sec = Some(30);
            let scope = CompletedRunVerifyScope {
                scope_id: "run-verify-failure".to_string(),
                scope_kind: RunVerifyScopeKind::Run,
                subrun_id: None,
            };

            execute_run_verification(temp_dir.path(), &mut run, &scope).await;

            let verify = run
                .dispatch
                .latest_run_verify
                .as_ref()
                .expect("verification result");
            assert!(!verify.ok);
            assert_eq!(verify.scope_id, "run-verify-failure");
            assert_eq!(run.phase, RunPhase::Failed);
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
        if let reliability::RuntimeState::Recoverable { retry_after, .. } = &mut task.runtime.state
        {
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
    fn normalize_command_type_orchestration_requires_canonical_names() {
        assert_eq!(
            normalize_command_type("orchestration.start_run"),
            "orchestration.start_run"
        );
        assert_eq!(
            normalize_command_type("orchestration.accept_plan"),
            "orchestration.accept_plan"
        );
        assert_eq!(
            normalize_command_type("orchestration.get_run"),
            "orchestration.get_run"
        );
        assert_eq!(
            normalize_command_type("orchestration.dispatch_run"),
            "orchestration.dispatch_run"
        );
        assert_eq!(
            normalize_command_type("orchestration.cancel_run"),
            "orchestration.cancel_run"
        );
        assert_eq!(
            normalize_command_type("orchestration.resume_run"),
            "orchestration.resume_run"
        );

        assert_eq!(
            normalize_command_type("orchestration.startRun"),
            "orchestration.startRun"
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
    fn runtime_selected_model_entry_falls_back_to_ad_hoc_session_model() {
        let options = rpc_options_with_models(Vec::new());
        let mut session = Session::in_memory();
        session.header.provider = Some("openai".to_string());
        session.header.model_id = Some("gpt-4.1".to_string());

        let resolved =
            runtime_selected_model_entry(&session, &options).expect("resolve ad hoc model");
        assert_eq!(resolved.model.provider, "openai");
        assert_eq!(resolved.model.id, "gpt-4.1");
    }

    #[test]
    fn runtime_provider_model_entry_synthesizes_unknown_provider_metadata() {
        let resolved =
            runtime_provider_model_entry("test-provider", "test-model").expect("synthetic model");
        assert_eq!(resolved.model.provider, "test-provider");
        assert_eq!(resolved.model.id, "test-model");
        assert_eq!(resolved.model.api, "unknown");
        assert_eq!(resolved.model.input, vec![crate::provider::InputType::Text]);
        assert!(resolved.model.reasoning);
    }

    fn sample_start_run_request() -> StartRunRequest {
        StartRunRequest {
            run_id: Some("run-123".to_string()),
            objective: "Ship runtime bootstrap".to_string(),
            tasks: vec![
                TaskContract {
                    task_id: "task-a".to_string(),
                    objective: "Build the runtime snapshot".to_string(),
                    parent_goal_trace_id: Some("goal-1".to_string()),
                    invariants: vec!["keep it durable".to_string()],
                    max_touched_files: Some(3),
                    forbid_paths: vec!["target/".to_string()],
                    verify_command: String::new(),
                    verify_timeout_sec: None,
                    max_attempts: Some(2),
                    input_snapshot: None,
                    acceptance_ids: vec!["acc-a".to_string()],
                    planned_touches: vec!["src/runtime/controller.rs".to_string()],
                    prerequisites: Vec::new(),
                    enforce_symbol_drift_check: false,
                },
                TaskContract {
                    task_id: "task-b".to_string(),
                    objective: "Persist runtime events".to_string(),
                    parent_goal_trace_id: None,
                    invariants: Vec::new(),
                    max_touched_files: None,
                    forbid_paths: Vec::new(),
                    verify_command: "cargo test runtime_store".to_string(),
                    verify_timeout_sec: Some(90),
                    max_attempts: None,
                    input_snapshot: None,
                    acceptance_ids: vec!["acc-b".to_string()],
                    planned_touches: vec!["src/runtime/store.rs".to_string()],
                    prerequisites: vec![TaskPrerequisite {
                        task_id: "task-a".to_string(),
                        trigger: reliability::EdgeTrigger::OnSuccess,
                    }],
                    enforce_symbol_drift_check: false,
                },
            ],
            run_verify_command: "cargo test runtime".to_string(),
            run_verify_timeout_sec: Some(45),
            max_parallelism: Some(2),
        }
    }

    #[test]
    fn build_runtime_plan_artifact_collects_tasks_and_verify_commands() {
        let request = sample_start_run_request();
        let plan = build_runtime_plan_artifact("run-123", &request);

        assert_eq!(plan.plan_id, "run-123-plan");
        assert_eq!(plan.task_drafts.len(), 2);
        assert!(plan.digest.len() >= 32);
        assert!(
            plan.test_strategy
                .iter()
                .any(|command| command == "cargo test runtime")
        );
        assert!(
            plan.test_strategy
                .iter()
                .any(|command| command == "cargo test runtime_store")
        );
    }

    #[test]
    fn build_runtime_task_nodes_preserves_dependencies_and_verify_defaults() {
        let request = sample_start_run_request();
        let tasks = build_runtime_task_nodes(&request, 45);

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].spec.verify.command, "cargo test runtime");
        assert_eq!(tasks[0].spec.verify.timeout_sec, 45);
        assert_eq!(tasks[0].spec.constraints.max_touched_files, Some(3));
        assert_eq!(tasks[1].deps, vec!["task-a".to_string()]);
        assert_eq!(tasks[1].spec.verify.command, "cargo test runtime_store");
        assert_eq!(tasks[1].spec.verify.timeout_sec, 90);
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
}
