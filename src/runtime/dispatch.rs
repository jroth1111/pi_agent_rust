use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::reliability;
use crate::runtime::execution::{DispatchGrant, RuntimeExecutionState};
use crate::runtime::scheduler;
use crate::runtime::store::RuntimeStore;
use crate::runtime::types::{LeaseRecord, RunPhase, RunSnapshot, TaskNode, TaskState};
use crate::runtime::verification::apply_run_verify_lifecycle;
use asupersync::sync::Mutex;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::runtime::types::{ExecutionTier, TaskReport};

pub const MAX_AUTOMATED_RECOVERABLE_WAIT: Duration = Duration::from_secs(60);
pub const ORCHESTRATION_ROLLBACK_RETRY_DELAY: Duration = Duration::from_millis(100);

#[async_trait]
pub trait DispatchRunHost: Send + Sync {
    async fn record_dispatch_grants(&self, grants: &[DispatchGrant]) -> Result<()>;

    async fn execute_dispatch_grants(
        &self,
        run: RunSnapshot,
        grants: &[DispatchGrant],
    ) -> Result<RunSnapshot>;
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

const fn failure_class_label(class: reliability::FailureClass) -> &'static str {
    match class {
        reliability::FailureClass::VerificationFailed => "verification_failed",
        reliability::FailureClass::ScopeCreepDetected => "scope_creep_detected",
        reliability::FailureClass::MergeConflict => "merge_conflict",
        reliability::FailureClass::HumanBlocker => "human_blocker",
        reliability::FailureClass::InfraTransient => "infra_transient",
        reliability::FailureClass::InfraPermanent => "infra_permanent",
        reliability::FailureClass::MaxAttemptsExceeded => "max_attempts_exceeded",
    }
}

pub fn sync_runtime_task_from_reliability(
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
        .map(std::path::PathBuf::from)
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

pub fn sync_runtime_snapshot_from_reliability(
    reliability: &RuntimeExecutionState,
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

pub fn apply_runtime_scheduler_lifecycle(snapshot: &mut RunSnapshot) {
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

pub const fn run_requires_plan_acceptance(snapshot: &RunSnapshot) -> bool {
    snapshot.plan_required && !snapshot.plan_accepted
}

pub fn recompute_runtime_task_counts(snapshot: &mut RunSnapshot) {
    let mut task_counts = BTreeMap::new();
    for task in snapshot.tasks.values() {
        *task_counts
            .entry(runtime_task_state_label(task.runtime.state).to_string())
            .or_insert(0) += 1;
    }
    snapshot.summary.task_counts = task_counts;
}

pub fn run_has_live_tasks(reliability: &RuntimeExecutionState, run: &RunSnapshot) -> bool {
    let task_ids = run.task_ids();
    !task_ids.is_empty()
        && task_ids
            .iter()
            .all(|task_id| reliability.tasks.contains_key(task_id))
}

pub fn refresh_live_run_from_reliability(
    reliability: &mut RuntimeExecutionState,
    run: &mut RunSnapshot,
) -> bool {
    let has_live_tasks = run_has_live_tasks(reliability, run);
    if has_live_tasks {
        reliability.refresh_dependency_states();
        refresh_run_from_reliability(reliability, run);
    }
    has_live_tasks
}

pub fn refresh_run_from_reliability(reliability: &RuntimeExecutionState, run: &mut RunSnapshot) {
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

pub fn dispatch_run_wave(
    reliability: &mut RuntimeExecutionState,
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

pub fn cancel_live_run_tasks(
    reliability: &mut RuntimeExecutionState,
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

pub fn next_recoverable_retry_delay(
    reliability: &RuntimeExecutionState,
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

pub fn build_runtime_task_report(
    reliability: &RuntimeExecutionState,
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
        failure_class: match &task.runtime.state {
            reliability::RuntimeState::Recoverable { reason, .. } => {
                Some(failure_class_label(*reason).to_string())
            }
            reliability::RuntimeState::AwaitingHuman { .. } => {
                Some(failure_class_label(reliability::FailureClass::HumanBlocker).to_string())
            }
            reliability::RuntimeState::Terminal(reliability::TerminalState::Failed {
                class,
                ..
            }) => Some(failure_class_label(*class).to_string()),
            reliability::RuntimeState::Terminal(reliability::TerminalState::Canceled {
                ..
            }) => Some("canceled".to_string()),
            _ => None,
        },
        blockers: match &task.runtime.state {
            reliability::RuntimeState::Blocked { waiting_on } => waiting_on.clone(),
            reliability::RuntimeState::AwaitingHuman {
                question, context, ..
            } => vec![format!("{question}: {context}")],
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
        },
        workspace_snapshot: task.spec.input_snapshot.clone(),
        generated_at: chrono::Utc::now(),
    })
}

pub async fn dispatch_run_until_quiescent(
    cx: &AgentCx,
    reliability_state: &Arc<Mutex<RuntimeExecutionState>>,
    runtime_store: &RuntimeStore,
    host: &dyn DispatchRunHost,
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
            if run_has_live_tasks(&rel, &run) {
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

        host.record_dispatch_grants(&grants).await?;
        let run_id = run.spec.run_id.clone();
        run = match host.execute_dispatch_grants(run, &grants).await {
            Ok(updated_run) => updated_run,
            Err(err) if grants.len() == 1 => {
                let recovered_run = runtime_store.load_snapshot(&run_id)?;
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
