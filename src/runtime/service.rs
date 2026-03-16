use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::runtime::controller::{ControllerOutput, RuntimeCommand, RuntimeController};
use crate::runtime::dispatch::{
    DispatchRunHost, build_runtime_task_report, dispatch_run_until_quiescent,
    refresh_live_run_from_reliability, refresh_run_from_reliability, run_has_live_tasks,
    run_requires_plan_acceptance,
};
use crate::runtime::execution::{
    AppendEvidenceRequest, BlockerReport, DispatchGrant, RuntimeExecutionState, SubmitTaskRequest,
    SubmitTaskResponse, TaskContract,
};
use crate::runtime::model_routing::{ModelRoute, PhaseModelRouter};
use crate::runtime::policy::PolicySet;
use crate::runtime::reliability;
use crate::runtime::store::RuntimeStore;
use crate::runtime::types::{
    AutonomyLevel, ExecutionTier, ModelProfile, PlanArtifact, RunBudgets, RunConstraints, RunPhase,
    RunSnapshot, RunSpec, TaskConstraints, TaskNode, TaskReport, TaskSpec, VerifySpec,
};
use crate::runtime::{
    CompletedRunVerifyScope, completed_run_verify_scope, completed_scope_from_run_verify,
    execute_run_verification, should_skip_run_verify,
};
use asupersync::sync::Mutex;
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStartRunRequest {
    pub objective: String,
    pub tasks: Vec<TaskContract>,
    pub run_verify_command: String,
    #[serde(default)]
    pub run_verify_timeout_sec: Option<u32>,
    #[serde(default)]
    pub max_parallelism: Option<usize>,
}

#[async_trait]
pub trait RuntimeServiceHost: DispatchRunHost + Send + Sync {
    fn runtime_store(&self) -> &RuntimeStore;

    fn reliability_state(&self) -> &Arc<Mutex<RuntimeExecutionState>>;

    async fn current_model_profile(&self, cx: &AgentCx) -> Result<ModelProfile>;

    async fn workspace_root(&self, cx: &AgentCx) -> Result<PathBuf>;

    async fn persist_output(&self, cx: &AgentCx, output: &ControllerOutput) -> Result<()>;

    async fn persist_snapshot(&self, cx: &AgentCx, snapshot: &RunSnapshot) -> Result<()>;

    async fn persist_evidence_record(
        &self,
        cx: &AgentCx,
        evidence: &reliability::EvidenceRecord,
    ) -> Result<()>;

    async fn persist_submit_task_result(
        &self,
        cx: &AgentCx,
        result: &SubmitTaskResponse,
    ) -> Result<()>;

    async fn persist_blocker_resolution(
        &self,
        cx: &AgentCx,
        req: &BlockerReport,
        state: &str,
    ) -> Result<()>;

    async fn cancel_live_run_tasks_and_sync(
        &self,
        cx: &AgentCx,
        run: &mut RunSnapshot,
    ) -> Result<()>;
}

pub fn ensure_run_id_available(runtime_store: &RuntimeStore, run_id: &str) -> Result<()> {
    if runtime_store.exists(run_id) {
        return Err(Error::validation(format!(
            "orchestration run already exists: {run_id}"
        )));
    }
    Ok(())
}

pub fn build_runtime_plan_artifact(run_id: &str, req: &RuntimeStartRunRequest) -> PlanArtifact {
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

pub fn build_runtime_task_nodes(
    req: &RuntimeStartRunRequest,
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

pub fn select_execution_tier(tasks: &[TaskContract]) -> ExecutionTier {
    match tasks.len() {
        0 | 1 => ExecutionTier::Inline,
        2..=24 if orchestration_dag_depth(tasks) <= 4 => ExecutionTier::Wave,
        _ => ExecutionTier::Hierarchical,
    }
}

pub fn next_run_id(candidate: Option<&str>) -> String {
    candidate
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4().simple()))
}

pub fn validate_start_run_request(req: &RuntimeStartRunRequest) -> Result<()> {
    if req.tasks.is_empty() {
        return Err(Error::validation("Run must include at least one task"));
    }
    if req.objective.trim().is_empty() {
        return Err(Error::validation("Run objective cannot be empty"));
    }
    if req.run_verify_command.trim().is_empty() {
        return Err(Error::validation("runVerifyCommand cannot be empty"));
    }
    if matches!(req.run_verify_timeout_sec, Some(0)) {
        return Err(Error::validation(
            "runVerifyTimeoutSec must be at least 1 when provided",
        ));
    }
    if matches!(req.max_parallelism, Some(0)) {
        return Err(Error::validation(
            "maxParallelism must be at least 1 when provided",
        ));
    }
    Ok(())
}

pub fn bootstrap_run(
    run_id: &str,
    req: &RuntimeStartRunRequest,
    model_profile: ModelProfile,
    root_workspace: PathBuf,
    default_verify_timeout_sec: u32,
) -> Result<ControllerOutput> {
    let selected_tier = select_execution_tier(&req.tasks);
    let run_verify_timeout_sec = req
        .run_verify_timeout_sec
        .unwrap_or(default_verify_timeout_sec);
    let spec = RunSpec {
        run_id: run_id.to_string(),
        objective: req.objective.clone(),
        root_workspace,
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

pub fn accept_plan(snapshot: RunSnapshot, model_profile: ModelProfile) -> Result<ControllerOutput> {
    let route = ModelRoute::from_profile(&model_profile);
    let mut controller =
        RuntimeController::from_snapshot(snapshot, PhaseModelRouter::new(route), PolicySet::new());
    controller
        .handle(RuntimeCommand::AcceptPlan)
        .map_err(|err| Error::session(format!("plan acceptance failed: {err}")))
}

pub async fn start_run<H: RuntimeServiceHost + ?Sized>(
    cx: &AgentCx,
    host: &H,
    req: &RuntimeStartRunRequest,
    requested_run_id: Option<&str>,
    default_verify_timeout_sec: u32,
) -> Result<RunSnapshot> {
    validate_start_run_request(req)?;
    let run_id = next_run_id(requested_run_id);
    ensure_run_id_available(host.runtime_store(), &run_id)?;

    {
        let mut rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        for contract in &req.tasks {
            rel.get_or_create_task(contract)?;
        }
        for contract in &req.tasks {
            rel.reconcile_prerequisites(contract)?;
        }
        rel.refresh_dependency_states();
    }

    let output = bootstrap_run(
        &run_id,
        req,
        host.current_model_profile(cx).await?,
        host.workspace_root(cx).await?,
        default_verify_timeout_sec,
    )?;
    host.persist_output(cx, &output).await?;
    Ok(output.snapshot)
}

pub async fn append_evidence<H: RuntimeServiceHost + ?Sized>(
    cx: &AgentCx,
    host: &H,
    req: AppendEvidenceRequest,
) -> Result<reliability::EvidenceRecord> {
    let evidence = {
        let mut rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        rel.append_evidence(req)?
    };

    host.persist_evidence_record(cx, &evidence).await?;
    Ok(evidence)
}

pub async fn submit_task<H: RuntimeServiceHost + ?Sized>(
    cx: &AgentCx,
    host: &H,
    req: SubmitTaskRequest,
) -> Result<SubmitTaskResponse> {
    let req_for_report = req.clone();
    let result = {
        let mut rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        rel.submit_task(req)?
    };

    host.persist_submit_task_result(cx, &result).await?;

    let report = {
        let rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        build_submit_task_report(&rel, &req_for_report, &result)?
    };

    sync_task_runs(cx, host, &result.task_id, Some(report)).await?;
    Ok(result)
}

pub async fn resolve_blocker<H: RuntimeServiceHost + ?Sized>(
    cx: &AgentCx,
    host: &H,
    req: BlockerReport,
) -> Result<String> {
    let state = {
        let mut rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        rel.resolve_blocker(req.clone())?
    };

    host.persist_blocker_resolution(cx, &req, &state).await?;

    let report = {
        let rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        let summary = if req.resolved {
            format!("Blocker resolved: state={state}")
        } else {
            format!("Human blocker raised: state={state}")
        };
        build_runtime_task_report(&rel, &req.task_id, summary)
    };

    sync_task_runs(cx, host, &req.task_id, report).await?;
    Ok(state)
}

pub async fn accept_run_plan<H: RuntimeServiceHost + ?Sized>(
    cx: &AgentCx,
    host: &H,
    run_id: &str,
) -> Result<RunSnapshot> {
    let run = host
        .runtime_store()
        .load_snapshot(run_id)
        .map_err(|_| Error::session(format!("orchestration run not found: {run_id}")))?;

    if run.plan.is_none() {
        return Err(Error::session(format!(
            "orchestration run {run_id} does not have a materialized plan"
        )));
    }

    let output = if run.plan_accepted {
        ControllerOutput {
            snapshot: run,
            events: Vec::new(),
            transitions: Vec::new(),
            routed_phases: Vec::new(),
            policy_decisions: Vec::new(),
        }
    } else {
        accept_plan(run, host.current_model_profile(cx).await?)?
    };

    host.persist_output(cx, &output).await?;
    Ok(output.snapshot)
}

pub async fn dispatch_run<H: RuntimeServiceHost>(
    cx: &AgentCx,
    host: &H,
    run_id: &str,
    agent_id_prefix: &str,
    lease_ttl_sec: i64,
) -> Result<(RunSnapshot, Vec<DispatchGrant>)> {
    let run = host
        .runtime_store()
        .load_snapshot(run_id)
        .map_err(|_| Error::session(format!("orchestration run not found: {run_id}")))?;

    if run_requires_plan_acceptance(&run) {
        return Err(Error::validation(format!(
            "Run {} is still in planning; accept the plan before dispatching",
            run.spec.run_id
        )));
    }

    let (run, grants) = dispatch_run_until_quiescent(
        cx,
        host.reliability_state(),
        host.runtime_store(),
        host,
        run,
        agent_id_prefix,
        lease_ttl_sec,
    )
    .await?;
    host.persist_snapshot(cx, &run).await?;
    Ok((run, grants))
}

pub async fn cancel_run<H: RuntimeServiceHost + ?Sized>(
    cx: &AgentCx,
    host: &H,
    run_id: &str,
) -> Result<RunSnapshot> {
    let mut run = host
        .runtime_store()
        .load_snapshot(run_id)
        .map_err(|_| Error::session(format!("orchestration run not found: {run_id}")))?;

    let has_live_tasks = {
        let rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        run_has_live_tasks(&rel, &run)
    };

    if has_live_tasks {
        host.cancel_live_run_tasks_and_sync(cx, &mut run).await?;
    } else {
        run.phase = RunPhase::Canceled;
        run.dispatch.active_wave = None;
        run.dispatch.active_subrun_id = None;
        run.touch();
    }

    host.persist_snapshot(cx, &run).await?;
    Ok(run)
}

pub async fn resume_run<H: RuntimeServiceHost>(
    cx: &AgentCx,
    host: &H,
    run_id: &str,
) -> Result<RunSnapshot> {
    let mut run = host
        .runtime_store()
        .load_snapshot(run_id)
        .map_err(|_| Error::session(format!("orchestration run not found: {run_id}")))?;

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
        let cwd = host.workspace_root(cx).await?;
        execute_run_verification(&cwd, &mut run, &scope).await;
    }

    let has_live_tasks = {
        let mut rel = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        refresh_live_run_from_reliability(&mut rel, &mut run)
    };

    if has_live_tasks
        && !matches!(
            run.phase,
            RunPhase::Canceled | RunPhase::Failed | RunPhase::Completed | RunPhase::AwaitingHuman
        )
    {
        let (updated_run, _) = dispatch_run_until_quiescent(
            cx,
            host.reliability_state(),
            host.runtime_store(),
            host,
            run,
            "resume",
            3600,
        )
        .await?;
        run = updated_run;
    } else {
        run.touch();
    }

    host.persist_snapshot(cx, &run).await?;
    Ok(run)
}

pub(crate) fn build_submit_task_report(
    reliability: &RuntimeExecutionState,
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

    let reliability::VerifyPlan::Standard { command, .. } = &task.spec.verify;

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
        verify_command: command.clone(),
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

fn failure_class_label(class: reliability::FailureClass) -> &'static str {
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

fn refresh_task_runs_with_verify_scopes(
    reliability: &RuntimeExecutionState,
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
        refresh_run_from_reliability(reliability, &mut run);
        let verify_scope =
            completed_run_verify_scope(&run).filter(|scope| !should_skip_run_verify(&run, scope));
        updated_runs.push((run, verify_scope));
    }

    updated_runs
}

pub(crate) fn refresh_task_runs(
    reliability: &RuntimeExecutionState,
    runtime_store: &RuntimeStore,
    task_id: &str,
    report: Option<TaskReport>,
) -> Vec<RunSnapshot> {
    refresh_task_runs_with_verify_scopes(reliability, runtime_store, task_id, report.as_ref())
        .into_iter()
        .map(|(run, _)| run)
        .collect()
}

pub(crate) async fn sync_task_runs<H: RuntimeServiceHost + ?Sized>(
    cx: &AgentCx,
    host: &H,
    task_id: &str,
    report: Option<TaskReport>,
) -> Result<Vec<RunSnapshot>> {
    let refreshed_runs = {
        let reliability = host
            .reliability_state()
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        refresh_task_runs_with_verify_scopes(
            &reliability,
            host.runtime_store(),
            task_id,
            report.as_ref(),
        )
    };

    let cwd = host.workspace_root(cx).await?;
    let mut updated_runs = Vec::with_capacity(refreshed_runs.len());
    for (mut run, verify_scope) in refreshed_runs {
        if let Some(scope) = verify_scope {
            execute_run_verification(&cwd, &mut run, &scope).await;
        }
        host.persist_snapshot(cx, &run).await?;
        updated_runs.push(run);
    }
    Ok(updated_runs)
}

fn runtime_plan_digest(req: &RuntimeStartRunRequest) -> String {
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
