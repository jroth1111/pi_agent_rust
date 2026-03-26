use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::agent_cx::AgentCx;
use crate::compaction::ResolvedCompactionSettings;
use crate::config::Config;
use crate::contracts::dto::SessionIdentity;
use crate::error::{Error, Result};
use crate::model::{AssistantMessage, ContentBlock};
use crate::orchestration::{
    ExecutionTier, FlockWorkspace, RunLifecycle, RunStatus, RunStore, RunVerifyScopeKind,
    RunVerifyStatus, SubrunPlan, TaskReport, WaveStatus,
};
use crate::provider::Provider;
use crate::reliability::verifier::Verifier;
use crate::services::reliability_service::ReliabilityService;
use crate::session::Session;
use crate::surface::rpc_types::{
    AppendEvidenceRequest, CancelRunRequest, DispatchGrant, DispatchRunRequest, EvidenceRecord,
    RpcOrchestrationState, RpcReliabilityState, RunLookupRequest, StartRunRequest,
    SubmitTaskRequest, SubmitTaskResponse, TaskContract,
};
use asupersync::sync::Mutex;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletedRunVerifyScope {
    pub(crate) scope_id: String,
    pub(crate) scope_kind: RunVerifyScopeKind,
    pub(crate) subrun_id: Option<String>,
}

pub(crate) fn task_terminal_success(reliability: &RpcReliabilityState, task_id: &str) -> bool {
    reliability.tasks.get(task_id).is_some_and(|task| {
        matches!(
            task.runtime.state,
            crate::reliability::RuntimeState::Terminal(
                crate::reliability::TerminalState::Succeeded { .. }
                    | crate::reliability::TerminalState::Superseded { .. }
            )
        )
    })
}

pub(crate) fn run_terminal_success(reliability: &RpcReliabilityState, run: &RunStatus) -> bool {
    !run.task_ids.is_empty()
        && run
            .task_ids
            .iter()
            .all(|task_id| task_terminal_success(reliability, task_id))
}

pub(crate) fn completed_run_verify_scope(
    reliability: &RpcReliabilityState,
    run: &RunStatus,
) -> Option<CompletedRunVerifyScope> {
    if run.run_verify_command.trim().is_empty() || !run_terminal_success(reliability, run) {
        return None;
    }
    Some(CompletedRunVerifyScope {
        scope_id: run.run_id.clone(),
        scope_kind: RunVerifyScopeKind::Run,
        subrun_id: None,
    })
}

pub(crate) fn should_skip_run_verify(run: &RunStatus, scope: &CompletedRunVerifyScope) -> bool {
    run.latest_run_verify.as_ref().is_some_and(|status| {
        status.scope_id == scope.scope_id
            && status.scope_kind == scope.scope_kind
            && status.subrun_id == scope.subrun_id
    })
}

pub(crate) fn completed_scope_from_run_verify(status: &RunVerifyStatus) -> CompletedRunVerifyScope {
    CompletedRunVerifyScope {
        scope_id: status.scope_id.clone(),
        scope_kind: status.scope_kind,
        subrun_id: status.subrun_id.clone(),
    }
}

pub(crate) fn apply_run_verify_lifecycle(run: &mut RunStatus) {
    if !matches!(run.lifecycle, RunLifecycle::Canceled)
        && run
            .latest_run_verify
            .as_ref()
            .is_some_and(|status| !status.ok)
    {
        run.lifecycle = RunLifecycle::Failed;
    }
}

pub(crate) fn next_active_wave(
    existing: Option<WaveStatus>,
    mut active_task_ids: Vec<String>,
) -> Option<WaveStatus> {
    active_task_ids.sort();
    active_task_ids.dedup();
    if active_task_ids.is_empty() {
        return None;
    }

    let now = Utc::now();
    if let Some(existing) = existing {
        let mut existing_task_ids = existing.task_ids.clone();
        existing_task_ids.sort();
        existing_task_ids.dedup();
        if existing.completed_at.is_none() && existing_task_ids == active_task_ids {
            return Some(existing);
        }
    }

    Some(WaveStatus {
        wave_id: format!("wave-{}", now.timestamp_millis()),
        task_ids: active_task_ids,
        started_at: now,
        completed_at: None,
    })
}

const MAX_HIERARCHICAL_SUBRUN_TASKS: usize = 12;

pub(crate) fn topological_run_task_ids(
    reliability: &RpcReliabilityState,
    run: &RunStatus,
) -> Vec<String> {
    let run_task_ids = run.task_ids.iter().cloned().collect::<HashSet<_>>();
    let mut in_degree = run
        .task_ids
        .iter()
        .cloned()
        .map(|task_id| (task_id, 0usize))
        .collect::<HashMap<_, _>>();
    let mut adjacency = run
        .task_ids
        .iter()
        .cloned()
        .map(|task_id| (task_id, Vec::new()))
        .collect::<HashMap<_, Vec<String>>>();

    for edge in &reliability.edges {
        match &edge.kind {
            crate::reliability::EdgeKind::Prerequisite { .. }
                if run_task_ids.contains(&edge.from) && run_task_ids.contains(&edge.to) =>
            {
                adjacency
                    .entry(edge.from.clone())
                    .or_default()
                    .push(edge.to.clone());
                *in_degree.entry(edge.to.clone()).or_default() += 1;
            }
            _ => {}
        }
    }

    let mut ready = in_degree
        .iter()
        .filter_map(|(task_id, degree)| (*degree == 0).then_some(task_id.clone()))
        .collect::<Vec<_>>();
    ready.sort();

    let mut ordered = Vec::with_capacity(run.task_ids.len());
    while let Some(task_id) = ready.first().cloned() {
        ready.remove(0);
        ordered.push(task_id.clone());

        let mut neighbors = adjacency.get(&task_id).cloned().unwrap_or_default();
        neighbors.sort();
        for neighbor in neighbors {
            let Some(degree) = in_degree.get_mut(&neighbor) else {
                continue;
            };
            *degree = degree.saturating_sub(1);
            if *degree == 0 && !ordered.contains(&neighbor) && !ready.contains(&neighbor) {
                ready.push(neighbor);
            }
        }
        ready.sort();
    }

    let mut missing = run
        .task_ids
        .iter()
        .filter(|task_id| !ordered.contains(task_id))
        .cloned()
        .collect::<Vec<_>>();
    missing.sort();
    ordered.extend(missing);
    ordered
}

pub(crate) fn planned_subruns(
    reliability: &RpcReliabilityState,
    run: &RunStatus,
) -> Vec<SubrunPlan> {
    if run.selected_tier != ExecutionTier::Hierarchical {
        return Vec::new();
    }

    let mut adjacency = run
        .task_ids
        .iter()
        .cloned()
        .map(|task_id| (task_id, HashSet::new()))
        .collect::<HashMap<_, HashSet<String>>>();
    let run_task_ids = run.task_ids.iter().cloned().collect::<HashSet<_>>();

    for edge in &reliability.edges {
        match &edge.kind {
            crate::reliability::EdgeKind::Prerequisite { .. }
                if run_task_ids.contains(&edge.from) && run_task_ids.contains(&edge.to) =>
            {
                adjacency
                    .entry(edge.from.clone())
                    .or_default()
                    .insert(edge.to.clone());
                adjacency
                    .entry(edge.to.clone())
                    .or_default()
                    .insert(edge.from.clone());
            }
            _ => {}
        }
    }

    for (index, left_id) in run.task_ids.iter().enumerate() {
        let Some(left_task) = reliability.tasks.get(left_id) else {
            continue;
        };
        for right_id in run.task_ids.iter().skip(index + 1) {
            let Some(right_task) = reliability.tasks.get(right_id) else {
                continue;
            };
            let overlaps = left_task
                .spec
                .planned_touches
                .iter()
                .any(|path| right_task.spec.planned_touches.contains(path));
            if overlaps {
                adjacency
                    .entry(left_id.clone())
                    .or_default()
                    .insert(right_id.clone());
                adjacency
                    .entry(right_id.clone())
                    .or_default()
                    .insert(left_id.clone());
            }
        }
    }

    let topological_order = topological_run_task_ids(reliability, run);
    let order_index = topological_order
        .iter()
        .enumerate()
        .map(|(index, task_id)| (task_id.clone(), index))
        .collect::<HashMap<_, _>>();

    let mut visited = HashSet::new();
    let mut components = Vec::new();
    let mut seeds = run.task_ids.clone();
    seeds.sort_by_key(|task_id| order_index.get(task_id).copied().unwrap_or(usize::MAX));
    for seed in seeds {
        if !visited.insert(seed.clone()) {
            continue;
        }
        let mut component = vec![seed.clone()];
        let mut stack = vec![seed];
        while let Some(task_id) = stack.pop() {
            let mut neighbors = adjacency
                .get(&task_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            neighbors
                .sort_by_key(|neighbor| order_index.get(neighbor).copied().unwrap_or(usize::MAX));
            for neighbor in neighbors {
                if visited.insert(neighbor.clone()) {
                    stack.push(neighbor.clone());
                    component.push(neighbor);
                }
            }
        }
        components.push(component);
    }

    components.sort_by_key(|component| {
        component
            .iter()
            .filter_map(|task_id| order_index.get(task_id))
            .min()
            .copied()
            .unwrap_or(usize::MAX)
    });

    let mut planned = Vec::new();
    let mut subrun_index = 1usize;
    let mut pending_task_ids = Vec::new();
    for component in components {
        let component_task_ids = component.into_iter().collect::<HashSet<_>>();
        let ordered_component = topological_order
            .iter()
            .filter(|task_id| component_task_ids.contains(*task_id))
            .cloned()
            .collect::<Vec<_>>();
        if ordered_component.len() > MAX_HIERARCHICAL_SUBRUN_TASKS {
            if !pending_task_ids.is_empty() {
                planned.push(SubrunPlan {
                    subrun_id: format!("subrun-{subrun_index:02}"),
                    task_ids: std::mem::take(&mut pending_task_ids),
                });
                subrun_index += 1;
            }
            for chunk in ordered_component.chunks(MAX_HIERARCHICAL_SUBRUN_TASKS) {
                planned.push(SubrunPlan {
                    subrun_id: format!("subrun-{subrun_index:02}"),
                    task_ids: chunk.to_vec(),
                });
                subrun_index += 1;
            }
            continue;
        }

        if pending_task_ids.len() + ordered_component.len() > MAX_HIERARCHICAL_SUBRUN_TASKS
            && !pending_task_ids.is_empty()
        {
            planned.push(SubrunPlan {
                subrun_id: format!("subrun-{subrun_index:02}"),
                task_ids: std::mem::take(&mut pending_task_ids),
            });
            subrun_index += 1;
        }
        pending_task_ids.extend(ordered_component);
    }

    if !pending_task_ids.is_empty() {
        planned.push(SubrunPlan {
            subrun_id: format!("subrun-{subrun_index:02}"),
            task_ids: pending_task_ids,
        });
    }

    planned
}

pub(crate) fn derive_run_lifecycle(
    reliability: &RpcReliabilityState,
    task_ids: &[String],
) -> RunLifecycle {
    let mut saw_ready = false;
    let mut saw_in_flight = false;
    let mut saw_blocked = false;
    let mut saw_recoverable = false;
    let mut saw_awaiting_human = false;
    let mut saw_terminal_failure = false;
    let mut saw_terminal_success = false;

    for task_id in task_ids {
        let Some(task) = reliability.tasks.get(task_id) else {
            continue;
        };

        match &task.runtime.state {
            crate::reliability::RuntimeState::AwaitingHuman { .. } => saw_awaiting_human = true,
            crate::reliability::RuntimeState::Leased { .. }
            | crate::reliability::RuntimeState::Verifying { .. } => saw_in_flight = true,
            crate::reliability::RuntimeState::Recoverable { .. } => saw_recoverable = true,
            crate::reliability::RuntimeState::Blocked { .. } => saw_blocked = true,
            crate::reliability::RuntimeState::Ready => saw_ready = true,
            crate::reliability::RuntimeState::Terminal(
                crate::reliability::TerminalState::Succeeded { .. }
                | crate::reliability::TerminalState::Superseded { .. },
            ) => {
                saw_terminal_success = true;
            }
            crate::reliability::RuntimeState::Terminal(
                crate::reliability::TerminalState::Failed { .. }
                | crate::reliability::TerminalState::Canceled { .. },
            ) => {
                saw_terminal_failure = true;
            }
        }
    }

    if saw_awaiting_human {
        RunLifecycle::AwaitingHuman
    } else if saw_in_flight {
        RunLifecycle::Running
    } else if saw_ready || saw_recoverable {
        RunLifecycle::Pending
    } else if saw_blocked {
        RunLifecycle::Blocked
    } else if saw_terminal_failure {
        RunLifecycle::Failed
    } else if saw_terminal_success {
        RunLifecycle::Succeeded
    } else {
        RunLifecycle::Pending
    }
}

fn planned_wave_task_ids(
    reliability: &RpcReliabilityState,
    run: &RunStatus,
    candidate_task_ids: &[String],
) -> Vec<String> {
    let ready_task_ids = candidate_task_ids
        .iter()
        .filter_map(|task_id| {
            let task = reliability.tasks.get(task_id)?;
            if matches!(task.runtime.state, crate::reliability::RuntimeState::Ready) {
                Some(task_id.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let mut scheduled = Vec::new();
    let mut touched_paths = HashSet::new();
    let max_tasks = run.effective_max_parallelism();

    for task_id in ready_task_ids {
        if scheduled.len() >= max_tasks {
            break;
        }
        let Some(task) = reliability.tasks.get(&task_id) else {
            continue;
        };
        if task.spec.planned_touches.is_empty() {
            if scheduled.is_empty() {
                scheduled.push(task_id);
            }
            break;
        }

        let conflicts = task
            .spec
            .planned_touches
            .iter()
            .any(|path| touched_paths.contains(path));
        if conflicts {
            continue;
        }

        for path in &task.spec.planned_touches {
            touched_paths.insert(path.clone());
        }
        scheduled.push(task_id);
    }

    scheduled
}

pub(crate) fn run_has_live_tasks(reliability: &RpcReliabilityState, run: &RunStatus) -> bool {
    !run.task_ids.is_empty()
        && run
            .task_ids
            .iter()
            .all(|task_id| reliability.tasks.contains_key(task_id))
}

pub(crate) fn refresh_live_run_from_reliability(
    reliability: &mut RpcReliabilityState,
    run: &mut RunStatus,
) -> bool {
    let has_live_tasks = run_has_live_tasks(reliability, run);
    if has_live_tasks {
        refresh_run_from_reliability(reliability, run);
    }
    has_live_tasks
}

pub(crate) fn refresh_run_from_reliability(reliability: &RpcReliabilityState, run: &mut RunStatus) {
    let preserve_canceled = matches!(run.lifecycle, RunLifecycle::Canceled);
    run.task_counts = ReliabilityService::task_counts_for(reliability, &run.task_ids);
    run.planned_subruns = planned_subruns(reliability, run);

    if preserve_canceled {
        run.active_subrun_id = None;
        run.active_wave = None;
    } else {
        let active_scope_task_ids = if run.selected_tier == ExecutionTier::Hierarchical {
            let active_subrun = run.planned_subruns.iter().find(|subrun| {
                subrun.task_ids.iter().any(|task_id| {
                    reliability.tasks.get(task_id).is_some_and(|task| {
                        !matches!(
                            task.runtime.state,
                            crate::reliability::RuntimeState::Terminal(_)
                        )
                    })
                })
            });
            run.active_subrun_id = active_subrun.map(|subrun| subrun.subrun_id.clone());
            active_subrun
                .map(|subrun| subrun.task_ids.clone())
                .unwrap_or_default()
        } else {
            run.active_subrun_id = None;
            run.task_ids.clone()
        };

        let mut active_task_ids = active_scope_task_ids
            .iter()
            .filter_map(|task_id| {
                let task = reliability.tasks.get(task_id)?;
                match &task.runtime.state {
                    crate::reliability::RuntimeState::Leased { .. }
                    | crate::reliability::RuntimeState::Verifying { .. } => Some(task_id.clone()),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        if active_task_ids.is_empty() {
            active_task_ids = planned_wave_task_ids(reliability, run, &active_scope_task_ids);
        }
        run.active_wave = next_active_wave(run.active_wave.take(), active_task_ids);
    }

    if !preserve_canceled {
        run.lifecycle = derive_run_lifecycle(reliability, &run.task_ids);
    }
    apply_run_verify_lifecycle(run);
    run.touch();
}

pub(crate) async fn persist_run_status(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    run_store: &RunStore,
    status: &RunStatus,
) -> Result<()> {
    run_store.save(status)?;

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
            "orchestration_run_status".to_string(),
            Some(json!(status.clone())),
        );
    }
    guard.persist_session().await?;
    Ok(())
}

pub(crate) async fn session_workspace_root(
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

const fn failure_class_label(class: crate::reliability::FailureClass) -> &'static str {
    match class {
        crate::reliability::FailureClass::VerificationFailed => "verification_failed",
        crate::reliability::FailureClass::ScopeCreepDetected => "scope_creep_detected",
        crate::reliability::FailureClass::MergeConflict => "merge_conflict",
        crate::reliability::FailureClass::InfraTransient => "infra_transient",
        crate::reliability::FailureClass::InfraPermanent => "infra_permanent",
        crate::reliability::FailureClass::HumanBlocker => "human_blocker",
        crate::reliability::FailureClass::MaxAttemptsExceeded => "max_attempts_exceeded",
    }
}

fn runtime_failure_class_label(state: &crate::reliability::RuntimeState) -> Option<String> {
    match state {
        crate::reliability::RuntimeState::Recoverable { reason, .. } => {
            Some(failure_class_label(*reason).to_string())
        }
        crate::reliability::RuntimeState::AwaitingHuman { .. } => {
            Some(failure_class_label(crate::reliability::FailureClass::HumanBlocker).to_string())
        }
        crate::reliability::RuntimeState::Terminal(crate::reliability::TerminalState::Failed {
            class,
            ..
        }) => Some(failure_class_label(*class).to_string()),
        _ => None,
    }
}

fn task_report_blockers(state: &crate::reliability::RuntimeState) -> Vec<String> {
    match state {
        crate::reliability::RuntimeState::Blocked { waiting_on } => waiting_on.clone(),
        crate::reliability::RuntimeState::AwaitingHuman {
            question, context, ..
        } => {
            let mut blockers = vec![question.clone()];
            if !context.trim().is_empty() {
                blockers.push(context.clone());
            }
            blockers
        }
        crate::reliability::RuntimeState::Recoverable {
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

pub(crate) fn build_submit_task_report(
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
        crate::reliability::RuntimeState::Terminal(
            crate::reliability::TerminalState::Succeeded { .. },
        ) => task.runtime.attempt.saturating_add(1).max(1),
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
        crate::reliability::VerifyPlan::Standard { command, .. } => command.clone(),
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
        generated_at: Utc::now(),
    })
}

pub(crate) fn build_runtime_task_report(
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
        crate::reliability::VerifyPlan::Standard { command, .. } => command.clone(),
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
        generated_at: Utc::now(),
    })
}

pub(crate) fn refresh_task_runs_with_verify_scopes(
    reliability: &RpcReliabilityState,
    orchestration: &mut RpcOrchestrationState,
    task_id: &str,
    report: Option<&TaskReport>,
) -> Vec<(RunStatus, Option<CompletedRunVerifyScope>)> {
    let run_ids = orchestration.run_ids_for_task(task_id);
    let mut updated_runs = Vec::new();

    for run_id in run_ids {
        let Some(mut run) = orchestration.get_run(&run_id) else {
            continue;
        };
        if let Some(report) = report.filter(|report| run.task_ids.contains(&report.task_id)) {
            run.upsert_task_report(report.clone());
        }
        let verify_scope = completed_run_verify_scope(reliability, &run)
            .filter(|scope| !should_skip_run_verify(&run, scope));
        refresh_run_from_reliability(reliability, &mut run);
        orchestration.update_run(run.clone());
        updated_runs.push((run, verify_scope));
    }

    updated_runs
}

pub(crate) fn refresh_task_runs(
    reliability: &RpcReliabilityState,
    orchestration: &mut RpcOrchestrationState,
    task_id: &str,
    report: Option<TaskReport>,
) -> Vec<RunStatus> {
    refresh_task_runs_with_verify_scopes(reliability, orchestration, task_id, report.as_ref())
        .into_iter()
        .map(|(run, _)| run)
        .collect()
}

pub(crate) fn run_verify_scope_summary(
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

pub(crate) async fn execute_run_verification(
    cwd: &Path,
    run: &mut RunStatus,
    scope: &CompletedRunVerifyScope,
) {
    let timeout_sec = run.run_verify_timeout_sec.unwrap_or(60);
    let verify_status =
        match Verifier::execute_verify_command(cwd, &run.run_verify_command, timeout_sec).await {
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
                    generated_at: Utc::now(),
                }
            }
            Err(err) => RunVerifyStatus {
                scope_id: scope.scope_id.clone(),
                scope_kind: scope.scope_kind,
                subrun_id: scope.subrun_id.clone(),
                command: run.run_verify_command.clone(),
                timeout_sec,
                exit_code: -1,
                ok: false,
                summary: run_verify_scope_summary(scope, false, err.to_string()),
                duration_ms: 0,
                generated_at: Utc::now(),
            },
        };

    run.latest_run_verify = Some(verify_status);
    apply_run_verify_lifecycle(run);
    run.touch();
}

pub(crate) async fn sync_task_runs(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    task_id: &str,
    report: Option<TaskReport>,
) -> Result<Vec<RunStatus>> {
    let refreshed_runs = {
        let reliability = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        let mut orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        refresh_task_runs_with_verify_scopes(
            &reliability,
            &mut orchestration,
            task_id,
            report.as_ref(),
        )
    };

    let cwd = session_workspace_root(cx, session).await?;
    let mut updated_runs = Vec::with_capacity(refreshed_runs.len());
    for (mut run, verify_scope) in refreshed_runs {
        if let Some(scope) = verify_scope {
            execute_run_verification(&cwd, &mut run, &scope).await;
            let mut orchestration = orchestration_state
                .lock(cx)
                .await
                .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
            orchestration.update_run(run.clone());
        }
        persist_run_status(cx, session, run_store, &run).await?;
        updated_runs.push(run);
    }
    Ok(updated_runs)
}

pub(crate) async fn refresh_run_if_live(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    mut run: RunStatus,
) -> Result<RunStatus> {
    let original = run.clone();
    let has_live_tasks = {
        let mut reliability = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        refresh_live_run_from_reliability(&mut reliability, &mut run)
    };

    {
        let mut orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.update_run(run.clone());
    }

    if has_live_tasks && run != original {
        persist_run_status(cx, session, run_store, &run).await?;
    }

    Ok(run)
}

pub(crate) async fn current_session_identity(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
) -> Result<SessionIdentity> {
    let guard = session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("session lock failed: {err}")))?;
    let inner = guard
        .session
        .lock(cx)
        .await
        .map_err(|err| Error::session(format!("inner session lock failed: {err}")))?;
    Ok(SessionIdentity {
        session_id: inner.header.id.clone(),
        name: None,
        path: inner.path.as_ref().map(|path| path.display().to_string()),
    })
}

pub(crate) async fn append_evidence_record(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    session_identity: &SessionIdentity,
    req: AppendEvidenceRequest,
) -> Result<EvidenceRecord> {
    let req_for_identity = req.clone();
    let evidence = {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        ReliabilityService::append_evidence_state(&mut rel, req)?
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
        if inner_session.header.id != session_identity.session_id {
            return Err(Error::validation(format!(
                "session identity mismatch for append_evidence: live session `{}` did not match typed session `{}`",
                inner_session.header.id, session_identity.session_id
            )));
        }
        inner_session.append_verification_evidence_entry(evidence.clone());
        inner_session.append_custom_entry(
            "workflow_mutation_identity".to_string(),
            Some(json!({
                "mutation": "reliability.append_evidence",
                "sessionIdentity": session_identity,
                "taskId": req_for_identity.task_id,
                "command": req_for_identity.command,
                "exitCode": req_for_identity.exit_code,
            })),
        );
    }
    guard.persist_session().await?;
    Ok(evidence)
}

pub(crate) async fn submit_task_and_sync(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    session_identity: &SessionIdentity,
    req: SubmitTaskRequest,
) -> Result<SubmitTaskResponse> {
    let req_for_report = req.clone();
    let result = {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        ReliabilityService::submit_task_state(&mut rel, req)?
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
            if inner_session.header.id != session_identity.session_id {
                return Err(Error::validation(format!(
                    "session identity mismatch for submit_task: live session `{}` did not match typed session `{}`",
                    inner_session.header.id, session_identity.session_id
                )));
            }
            inner_session
                .append_close_decision_entry(result.close_payload.clone(), result.close.clone());
            inner_session.append_task_transition_entry(
                result.task_id.clone(),
                None,
                result.state.clone(),
                Some(json!({
                    "mutation": "reliability.submit_task",
                    "sessionIdentity": session_identity,
                    "leaseId": req_for_report.lease_id.clone(),
                    "fenceToken": req_for_report.fence_token,
                    "verifyRunId": req_for_report.verify_run_id.clone(),
                })),
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
        orchestration_state,
        run_store,
        &result.task_id,
        Some(report),
    )
    .await?;
    Ok(result)
}

fn next_run_id(candidate: Option<&str>) -> String {
    candidate
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4().simple()))
}

fn ensure_run_id_available(
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

fn select_execution_tier(tasks: &[TaskContract]) -> ExecutionTier {
    match tasks.len() {
        0 | 1 => ExecutionTier::Inline,
        2..=24 if orchestration_dag_depth(tasks) <= 4 => ExecutionTier::Wave,
        _ => ExecutionTier::Hierarchical,
    }
}

pub(crate) async fn current_run_status(
    cx: &AgentCx,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    run_id: &str,
) -> Result<RunStatus> {
    if let Some(run) = {
        let orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.get_run(run_id)
    } {
        return Ok(run);
    }
    run_store.load(run_id)
}

async fn load_existing_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    run_id: &str,
) -> Result<RunStatus> {
    let run = {
        let orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.get_run(run_id)
    }
    .or_else(|| run_store.load(run_id).ok())
    .ok_or_else(|| Error::session(format!("orchestration run not found: {run_id}")))?;

    refresh_run_if_live(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        run,
    )
    .await
}

pub(crate) async fn start_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    req: StartRunRequest,
) -> Result<RunStatus> {
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

    let run_id = next_run_id(req.run_id.as_deref());
    {
        let orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        ensure_run_id_available(&orchestration, run_store, &run_id)?;
    }

    let selected_tier = select_execution_tier(&req.tasks);
    let task_ids = req
        .tasks
        .iter()
        .map(|task| task.task_id.clone())
        .collect::<Vec<_>>();
    let mut status = RunStatus::new(run_id, req.objective, selected_tier);
    status.lifecycle = RunLifecycle::Pending;
    status.run_verify_command = req.run_verify_command;
    status.run_verify_timeout_sec = Some(
        req.run_verify_timeout_sec
            .unwrap_or(config.reliability_verify_timeout_sec_default()),
    );
    status.max_parallelism = req
        .max_parallelism
        .unwrap_or(RunStatus::DEFAULT_MAX_PARALLELISM);
    status.task_ids = task_ids;

    {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        for contract in &req.tasks {
            ReliabilityService::get_or_create_task(&mut rel, contract)?;
        }
        for contract in &req.tasks {
            ReliabilityService::reconcile_prerequisites(&mut rel, contract)?;
        }
        ReliabilityService::refresh_dependency_states(&mut rel);
        refresh_run_from_reliability(&rel, &mut status);
    }

    {
        let mut orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.register_run(status.clone());
    }
    persist_run_status(cx, session, run_store, &status).await?;

    Ok(status)
}

pub(crate) async fn rpc_start_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    req: StartRunRequest,
) -> Result<Value> {
    let status = start_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        config,
        req,
    )
    .await?;
    Ok(json!({ "run": status }))
}

pub(crate) async fn get_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    req: RunLookupRequest,
) -> Result<RunStatus> {
    load_existing_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        &req.run_id,
    )
    .await
}

pub(crate) async fn rpc_get_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    req: RunLookupRequest,
) -> Result<Value> {
    let run = get_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        req,
    )
    .await?;
    Ok(json!({ "run": run }))
}

pub(crate) async fn dispatch_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    req: DispatchRunRequest,
) -> Result<(RunStatus, Vec<DispatchGrant>)> {
    let agent_id_prefix = req
        .agent_id_prefix
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("run");
    let lease_ttl_sec = req.lease_ttl_sec.unwrap_or(3600);
    let run = load_existing_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        &req.run_id,
    )
    .await?;

    let (run, grants) = dispatch_run_until_quiescent(
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
    .await?;
    persist_run_status(cx, session, run_store, &run).await?;
    Ok((run, grants))
}

pub(crate) async fn rpc_dispatch_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    req: DispatchRunRequest,
) -> Result<Value> {
    let (run, grants) = dispatch_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        config,
        req,
    )
    .await?;
    Ok(json!({ "run": run, "grants": grants }))
}

pub(crate) async fn cancel_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    req: CancelRunRequest,
) -> Result<RunStatus> {
    let mut run = load_existing_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        &req.run_id,
    )
    .await?;
    let has_live_tasks = {
        let rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        run_has_live_tasks(&rel, &run)
    };
    if has_live_tasks {
        cancel_live_run_tasks_and_sync(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            &mut run,
        )
        .await?;
    } else {
        run.lifecycle = RunLifecycle::Canceled;
        run.active_wave = None;
        run.active_subrun_id = None;
        run.touch();
    }
    {
        let mut orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.update_run(run.clone());
    }
    persist_run_status(cx, session, run_store, &run).await?;
    Ok(run)
}

pub(crate) async fn rpc_cancel_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    req: CancelRunRequest,
) -> Result<Value> {
    let run = cancel_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        req,
    )
    .await?;
    Ok(json!({ "run": run }))
}

pub(crate) async fn resume_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    req: RunLookupRequest,
) -> Result<RunStatus> {
    let mut run = load_existing_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        &req.run_id,
    )
    .await?;

    if matches!(
        run.lifecycle,
        RunLifecycle::Canceled | RunLifecycle::Blocked | RunLifecycle::Failed
    ) {
        run.lifecycle = RunLifecycle::Pending;
    }
    if let Some(status) = run.latest_run_verify.as_ref().filter(|status| !status.ok) {
        let scope = completed_scope_from_run_verify(status);
        let cwd = session_workspace_root(cx, session).await?;
        execute_run_verification(&cwd, &mut run, &scope).await;
    }
    let has_live_tasks = {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        refresh_live_run_from_reliability(&mut rel, &mut run)
    };
    if has_live_tasks
        && !matches!(
            run.lifecycle,
            RunLifecycle::Canceled
                | RunLifecycle::Failed
                | RunLifecycle::Succeeded
                | RunLifecycle::Blocked
                | RunLifecycle::AwaitingHuman
        )
    {
        run = dispatch_run_until_quiescent(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            config,
            run,
            "resume",
            3600,
        )
        .await?
        .0;
    } else {
        run.touch();
        let mut orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.update_run(run.clone());
    }
    persist_run_status(cx, session, run_store, &run).await?;
    Ok(run)
}

pub(crate) async fn rpc_resume_run(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    req: RunLookupRequest,
) -> Result<Value> {
    let run = resume_run(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        config,
        req,
    )
    .await?;
    Ok(json!({ "run": run }))
}

pub(crate) const ORCHESTRATION_ROLLBACK_RETRY_DELAY: Duration = Duration::from_millis(100);
pub(crate) const MAX_AUTOMATED_RECOVERABLE_WAIT: Duration = Duration::from_secs(60);

pub(crate) fn dispatch_run_wave(
    reliability: &mut RpcReliabilityState,
    run: &mut RunStatus,
    agent_id_prefix: &str,
    lease_ttl_sec: i64,
) -> Result<Vec<DispatchGrant>> {
    refresh_live_run_from_reliability(reliability, run);
    if matches!(
        run.lifecycle,
        RunLifecycle::Canceled | RunLifecycle::Failed | RunLifecycle::Succeeded
    ) {
        return Err(Error::validation(format!(
            "run {} is not dispatchable in lifecycle {:?}",
            run.run_id, run.lifecycle
        )));
    }

    let dispatchable_task_ids = run
        .active_wave
        .as_ref()
        .map(|wave| {
            wave.task_ids
                .iter()
                .filter_map(|task_id| {
                    let task = reliability.tasks.get(task_id)?;
                    if matches!(task.runtime.state, crate::reliability::RuntimeState::Ready) {
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
        match ReliabilityService::request_dispatch_existing_state(
            reliability,
            &task_id,
            &agent_id,
            lease_ttl_sec,
        ) {
            Ok(grant) => grants.push(grant),
            Err(err) => {
                for grant in &grants {
                    let _ = ReliabilityService::expire_dispatch_grant(reliability, grant);
                }
                ReliabilityService::refresh_dependency_states(reliability);
                refresh_run_from_reliability(reliability, run);
                return Err(err);
            }
        }
    }

    refresh_run_from_reliability(reliability, run);
    Ok(grants)
}

pub(crate) fn cancel_live_run_tasks(
    reliability: &mut RpcReliabilityState,
    run: &mut RunStatus,
) -> Vec<DispatchGrant> {
    let leased_grants = run
        .task_ids
        .iter()
        .filter_map(|task_id| {
            let task = reliability.tasks.get(task_id)?;
            match &task.runtime.state {
                crate::reliability::RuntimeState::Leased {
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
        let _ = ReliabilityService::expire_dispatch_grant(reliability, grant);
    }

    ReliabilityService::refresh_dependency_states(reliability);
    refresh_run_from_reliability(reliability, run);
    run.lifecycle = RunLifecycle::Canceled;
    run.active_wave = None;
    run.active_subrun_id = None;
    run.touch();
    leased_grants
}

pub(crate) fn build_cancel_run_task_report(
    reliability: &RpcReliabilityState,
    run_id: &str,
    task_id: &str,
) -> Option<TaskReport> {
    let task = reliability.tasks.get(task_id)?;
    let state = ReliabilityService::state_label(&task.runtime.state);
    build_runtime_task_report(
        reliability,
        task_id,
        format!("run {run_id} canceled dispatch for task {task_id}; task returned to {state}"),
    )
}

pub(crate) fn build_dispatch_rollback_task_report(
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

pub(crate) fn next_recoverable_retry_delay(
    reliability: &RpcReliabilityState,
    run: &RunStatus,
) -> Option<Duration> {
    let now = Utc::now();

    run.task_ids
        .iter()
        .filter_map(|task_id| reliability.tasks.get(task_id))
        .filter_map(|task| match &task.runtime.state {
            crate::reliability::RuntimeState::Recoverable {
                retry_after: Some(retry_after),
                ..
            } if *retry_after > now => (*retry_after - now).to_std().ok(),
            _ => None,
        })
        .min()
}

pub(crate) fn apply_dispatch_rollback_recovery(
    reliability: &mut RpcReliabilityState,
    grant: &DispatchGrant,
    failure_summary: Option<&str>,
) -> String {
    let _ = ReliabilityService::expire_dispatch_grant(reliability, grant);

    let fallback_state = "ready".to_string();
    let rollback_state = {
        let Some(task) = reliability.tasks.get_mut(&grant.task_id) else {
            return fallback_state;
        };

        if let Some(summary) = failure_summary {
            let failed_at = Utc::now();
            task.runtime.attempt = task.runtime.attempt.saturating_add(1);
            task.runtime.last_transition_at = failed_at;
            if task.runtime.attempt < task.spec.max_attempts {
                let retry_after =
                    chrono::Duration::from_std(ORCHESTRATION_ROLLBACK_RETRY_DELAY).ok();
                task.runtime.state = crate::reliability::RuntimeState::Recoverable {
                    reason: crate::reliability::FailureClass::InfraTransient,
                    failure_artifact: None,
                    handoff_summary: summary.to_string(),
                    retry_after: retry_after.map(|delay| failed_at + delay),
                };
            } else {
                task.runtime.state = crate::reliability::RuntimeState::Terminal(
                    crate::reliability::TerminalState::Failed {
                        class: crate::reliability::FailureClass::MaxAttemptsExceeded,
                        verify_run_id: None,
                        failed_at,
                    },
                );
            }
        }

        ReliabilityService::state_label(&task.runtime.state).to_string()
    };

    ReliabilityService::refresh_dependency_states(reliability);
    reliability
        .tasks
        .get(&grant.task_id)
        .map(|task| ReliabilityService::state_label(&task.runtime.state).to_string())
        .unwrap_or(rollback_state)
}

pub(crate) async fn append_dispatch_grants_session_entries(
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

pub(crate) async fn append_canceled_dispatch_grants_session_entries(
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
                        ReliabilityService::state_label(&task.runtime.state).to_string(),
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

pub(crate) async fn append_dispatch_rollback_session_entry(
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

pub(crate) async fn cancel_live_run_tasks_and_sync(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    run: &mut RunStatus,
) -> Result<()> {
    let canceled_grants = {
        let mut rel = reliability_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
        cancel_live_run_tasks(&mut rel, run)
    };

    {
        let mut orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.update_run(run.clone());
    }

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
            build_cancel_run_task_report(&rel, &run.run_id, &grant.task_id)
        };
        let updated_runs = sync_task_runs(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            &grant.task_id,
            report,
        )
        .await?;
        if let Some(updated_run) = updated_runs
            .into_iter()
            .find(|updated_run| updated_run.run_id == run.run_id)
        {
            *run = updated_run;
        }
    }

    run.lifecycle = RunLifecycle::Canceled;
    run.active_wave = None;
    run.active_subrun_id = None;
    run.touch();
    {
        let mut orchestration = orchestration_state
            .lock(cx)
            .await
            .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
        orchestration.update_run(run.clone());
    }
    persist_run_status(cx, session, run_store, run).await?;

    Ok(())
}

const INLINE_WORKER_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

#[derive(Debug)]
struct TaskVerificationOutcome {
    command: String,
    exit_code: i32,
    output: String,
    timed_out: bool,
    passed: bool,
}

#[async_trait]
pub(crate) trait OrchestrationInlineWorker: Send + Sync {
    async fn execute(&self, workspace_path: &Path, task: &TaskContract) -> Result<String>;
}

struct RpcSessionInlineWorker {
    provider: Arc<dyn Provider>,
    config: Config,
}

impl RpcSessionInlineWorker {
    fn new(provider: Arc<dyn Provider>, config: Config) -> Self {
        Self { provider, config }
    }
}

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

fn patch_digest_for_diff(diff: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(diff.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

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

fn task_contract_for_runtime_task(
    reliability: &RpcReliabilityState,
    task_id: &str,
) -> Result<TaskContract> {
    let task = reliability
        .tasks
        .get(task_id)
        .ok_or_else(|| Error::validation(format!("Unknown reliability task: {task_id}")))?;
    let (verify_command, verify_timeout_sec) = match &task.spec.verify {
        crate::reliability::VerifyPlan::Standard {
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

pub(crate) async fn rollback_dispatch_grant(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
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
        let mut run = {
            let Ok(orchestration) = orchestration_state.lock(cx).await else {
                return;
            };
            orchestration
                .get_run(run_id)
                .or_else(|| run_store.load(run_id).ok())
        };
        if let Some(mut run) = run.take() {
            if let Some(report) = rollback_report.as_ref() {
                run.upsert_task_report(report.clone());
            }
            refresh_run_from_reliability(&rel, &mut run);
            if let Ok(mut orchestration) = orchestration_state.lock(cx).await {
                orchestration.update_run(run.clone());
            }
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
        let _ = persist_run_status(cx, session, run_store, &run).await;
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

pub(crate) fn dispatch_workspace_segment_id(grant: &DispatchGrant) -> usize {
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

async fn finalize_captured_dispatch_execution(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    repo_root: &Path,
    run_id: &str,
    capture: CapturedDispatchExecution,
) -> Result<RunStatus> {
    let session_identity = current_session_identity(cx, session).await?;
    let evidence = match append_evidence_record(
        cx,
        session,
        reliability_state,
        &session_identity,
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
                orchestration_state,
                run_store,
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
            orchestration_state,
            run_store,
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
        .then(|| crate::reliability::ClosePayload {
            task_id: capture.contract.task_id.clone(),
            outcome: if trimmed_summary.is_empty() {
                format!("Completed {}", capture.contract.objective)
            } else {
                trimmed_summary.to_string()
            },
            outcome_kind: Some(crate::reliability::CloseOutcomeKind::Success),
            acceptance_ids: capture.contract.acceptance_ids.clone(),
            evidence_ids: vec![evidence.evidence_id.clone()],
            trace_parent: capture.contract.parent_goal_trace_id.clone(),
        });
    if let Err(err) = submit_task_and_sync(
        cx,
        session,
        reliability_state,
        orchestration_state,
        run_store,
        &session_identity,
        SubmitTaskRequest {
            task_id: capture.grant.task_id.clone(),
            lease_id: capture.grant.lease_id.clone(),
            fence_token: capture.grant.fence_token,
            patch_digest: capture.patch_digest,
            verify_run_id: format!("orchestration-inline:{run_id}:{}", capture.grant.task_id),
            verify_passed: Some(capture.verification.passed),
            verify_timed_out: capture.verification.timed_out,
            failure_class: (!capture.verification.passed)
                .then_some(crate::reliability::FailureClass::VerificationFailed),
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
            orchestration_state,
            run_store,
            run_id,
            &capture.grant,
            Some(summary),
        )
        .await;
        return Err(err);
    }

    current_run_status(cx, orchestration_state, run_store, run_id).await
}

pub(crate) async fn execute_inline_dispatch_grant_with_worker(
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
                    orchestration_state,
                    run_store,
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
        orchestration_state,
        run_store,
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
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    repo_root: &Path,
    run: RunStatus,
    grants: &[DispatchGrant],
    worker: &dyn OrchestrationInlineWorker,
) -> Result<RunStatus> {
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
                    orchestration_state,
                    run_store,
                    &updated_run.run_id,
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
            orchestration_state,
            run_store,
            repo_root,
            &updated_run.run_id,
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
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    repo_root: &Path,
    run: RunStatus,
    captures: Vec<CapturedDispatchExecution>,
) -> Result<RunStatus> {
    let mut updated_run = run;
    for capture in captures {
        updated_run = finalize_captured_dispatch_execution(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            repo_root,
            &updated_run.run_id,
            capture,
        )
        .await?;
    }
    Ok(updated_run)
}

pub(crate) async fn execute_dispatch_grants_with_worker(
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
                    orchestration_state,
                    run_store,
                    &run.run_id,
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
                        orchestration_state,
                        run_store,
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
                        orchestration_state,
                        run_store,
                        repo_root,
                        run,
                        captures,
                    )
                    .await?
                }
            } else {
                current_run_status(cx, orchestration_state, run_store, &run.run_id).await?
            };
            return Ok(updated_run);
        }

        if captured_dispatches_overlap(&captures) {
            return execute_dispatch_grants_sequentially_with_replay_base(
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
            .await;
        }

        return finalize_captured_dispatches(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
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
            orchestration_state,
            run_store,
            repo_root,
            &updated_run.run_id,
            grant,
            worker,
        )
        .await?;
    }
    Ok(updated_run)
}

pub(crate) async fn execute_inline_run_dispatch(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    run: RunStatus,
    grants: &[DispatchGrant],
) -> Result<RunStatus> {
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
        orchestration_state,
        run_store,
        repo_root.as_path(),
        run,
        grants,
        &worker,
    )
    .await
}

pub(crate) async fn dispatch_run_until_quiescent(
    cx: &AgentCx,
    session: &Arc<Mutex<AgentSession>>,
    reliability_state: &Arc<Mutex<RpcReliabilityState>>,
    orchestration_state: &Arc<Mutex<RpcOrchestrationState>>,
    run_store: &RunStore,
    config: &Config,
    mut run: RunStatus,
    agent_id_prefix: &str,
    lease_ttl_sec: i64,
) -> Result<(RunStatus, Vec<DispatchGrant>)> {
    loop {
        let mut grants = {
            let mut rel = reliability_state
                .lock(cx)
                .await
                .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
            let has_live_tasks = !run.task_ids.is_empty()
                && run
                    .task_ids
                    .iter()
                    .all(|task_id| rel.tasks.contains_key(task_id));
            if has_live_tasks {
                dispatch_run_wave(&mut rel, &mut run, agent_id_prefix, lease_ttl_sec)
            } else {
                Err(Error::session(format!(
                    "orchestration run {} is not live in this process",
                    run.run_id
                )))
            }
        }?;

        {
            let mut orchestration = orchestration_state
                .lock(cx)
                .await
                .map_err(|err| Error::session(format!("orchestration lock failed: {err}")))?;
            orchestration.update_run(run.clone());
        }

        if grants.is_empty() {
            let recoverable_wait = {
                let rel = reliability_state
                    .lock(cx)
                    .await
                    .map_err(|err| Error::session(format!("reliability lock failed: {err}")))?;
                if run_has_live_tasks(&rel, &run)
                    && matches!(run.lifecycle, RunLifecycle::Pending | RunLifecycle::Running)
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
        let run_id = run.run_id.clone();
        run = match execute_inline_run_dispatch(
            cx,
            session,
            reliability_state,
            orchestration_state,
            run_store,
            config,
            run,
            &grants,
        )
        .await
        {
            Ok(updated_run) => updated_run,
            Err(err) if grants.len() == 1 => {
                let recovered_run =
                    current_run_status(cx, orchestration_state, run_store, &run_id).await?;
                if matches!(
                    recovered_run.lifecycle,
                    RunLifecycle::Pending | RunLifecycle::Running
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
            run.lifecycle,
            RunLifecycle::Canceled
                | RunLifecycle::Failed
                | RunLifecycle::Succeeded
                | RunLifecycle::Blocked
                | RunLifecycle::AwaitingHuman
        ) {
            return Ok((run, grants));
        }
    }
}
