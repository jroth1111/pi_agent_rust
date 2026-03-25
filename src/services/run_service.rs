use crate::orchestration::{
    ExecutionTier, RunLifecycle, RunStatus, RunVerifyScopeKind, RunVerifyStatus, SubrunPlan,
    WaveStatus,
};
use crate::rpc::RpcReliabilityState;
use crate::services::reliability_service::ReliabilityService;
use chrono::Utc;
use std::collections::{HashMap, HashSet};

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
