use crate::runtime::events::RuntimeEventKind;
use crate::runtime::types::{
    ApprovalState, ContinuationReason, LeaseRecord, RunPhase, RunSnapshot, TaskNode, TaskState,
};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

pub fn maintenance_events(snapshot: &RunSnapshot, now: DateTime<Utc>) -> Vec<RuntimeEventKind> {
    let mut task_ids = snapshot.tasks.keys().cloned().collect::<Vec<_>>();
    task_ids.sort();

    let mut events = Vec::new();
    for task_id in task_ids {
        let Some(task) = snapshot.tasks.get(&task_id) else {
            continue;
        };
        let deps_satisfied = dependencies_satisfied(snapshot, task);
        match task.runtime.state {
            TaskState::Draft => events.push(task_state_change(
                &task_id,
                if deps_satisfied {
                    TaskState::Ready
                } else {
                    TaskState::Blocked
                },
                None,
                None,
                None,
                Some(ContinuationReason::TaskReady),
            )),
            TaskState::Blocked if deps_satisfied => events.push(task_state_change(
                &task_id,
                TaskState::Ready,
                None,
                None,
                None,
                Some(ContinuationReason::TaskReady),
            )),
            TaskState::Ready if !deps_satisfied => events.push(task_state_change(
                &task_id,
                TaskState::Blocked,
                None,
                Some("waiting for prerequisite completion".to_string()),
                None,
                Some(ContinuationReason::TaskReady),
            )),
            TaskState::Leased | TaskState::Executing
                if task
                    .runtime
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at <= now) =>
            {
                events.push(task_state_change(
                    &task_id,
                    TaskState::Recoverable,
                    None,
                    Some("task lease expired during runtime scheduling".to_string()),
                    Some(now),
                    Some(ContinuationReason::WakeTimer),
                ));
            }
            TaskState::Recoverable
                if task
                    .runtime
                    .retry_at
                    .is_some_and(|retry_at| retry_at <= now) =>
            {
                events.push(task_state_change(
                    &task_id,
                    if deps_satisfied {
                        TaskState::Ready
                    } else {
                        TaskState::Blocked
                    },
                    None,
                    None,
                    None,
                    Some(ContinuationReason::VerificationRetry),
                ));
            }
            _ => {}
        }
    }

    events
}

pub fn phase_and_wake_events(snapshot: &RunSnapshot, now: DateTime<Utc>) -> Vec<RuntimeEventKind> {
    if snapshot.phase.is_terminal() {
        return Vec::new();
    }

    if snapshot.plan_required && !snapshot.plan_accepted {
        return phase_event(snapshot, RunPhase::Planning, "accept plan before dispatch");
    }

    let approvals_pending = snapshot
        .approvals
        .values()
        .any(|approval| approval.state == ApprovalState::Pending);
    if approvals_pending {
        return phase_event(snapshot, RunPhase::AwaitingHuman, "await human approval");
    }

    let tasks = snapshot.tasks.values().collect::<Vec<_>>();
    if tasks.is_empty() {
        return Vec::new();
    }

    if tasks.iter().all(|task| task.runtime.state.is_terminal()) {
        return if tasks.iter().all(|task| {
            matches!(
                task.runtime.state,
                TaskState::Succeeded | TaskState::Superseded
            )
        }) {
            vec![RuntimeEventKind::RunCompleted]
        } else {
            vec![RuntimeEventKind::RunFailed {
                reason: "one or more tasks reached a failed terminal state".to_string(),
            }]
        };
    }

    if tasks
        .iter()
        .any(|task| task.runtime.state == TaskState::Verifying)
    {
        return phase_event(snapshot, RunPhase::Verifying, "verify current outputs");
    }

    if tasks
        .iter()
        .any(|task| matches!(task.runtime.state, TaskState::Leased | TaskState::Executing))
    {
        return phase_event(snapshot, RunPhase::Running, "continue active work");
    }

    if !snapshot.ready_queue.is_empty() {
        return phase_event(snapshot, RunPhase::Dispatching, "dispatch ready tasks");
    }

    let recoverable_retry_at = tasks
        .iter()
        .filter_map(|task| {
            (task.runtime.state == TaskState::Recoverable)
                .then_some(task.runtime.retry_at)
                .flatten()
        })
        .min();
    if tasks
        .iter()
        .any(|task| task.runtime.state == TaskState::Recoverable)
    {
        let mut events = phase_event(snapshot, RunPhase::Recovering, "retry recoverable work");
        if let Some(retry_at) = recoverable_retry_at.filter(|retry_at| *retry_at > now) {
            events.push(RuntimeEventKind::WakeScheduled {
                wake_at: retry_at,
                reason: "resume recoverable work".to_string(),
            });
        }
        return events;
    }

    if tasks.iter().any(|task| {
        matches!(
            task.runtime.state,
            TaskState::Blocked | TaskState::Draft | TaskState::AwaitingHuman
        )
    }) {
        return phase_event(
            snapshot,
            RunPhase::AwaitingHuman,
            "await dependency resolution",
        );
    }

    Vec::new()
}

pub fn rebuild_ready_queue(snapshot: &mut RunSnapshot) {
    let mut ready_task_ids = snapshot
        .tasks
        .iter()
        .filter_map(|(task_id, task)| {
            (task.runtime.state == TaskState::Ready && dependencies_satisfied(snapshot, task))
                .then_some(task_id.clone())
        })
        .collect::<Vec<_>>();
    ready_task_ids.sort();

    let mut scheduled = Vec::new();
    let mut touched_paths = HashSet::new();
    let max_tasks = snapshot.effective_max_parallelism();

    for task_id in ready_task_ids {
        if scheduled.len() >= max_tasks {
            break;
        }
        let Some(task) = snapshot.tasks.get(&task_id) else {
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

    snapshot.ready_queue = scheduled.into();
}

fn phase_event(snapshot: &RunSnapshot, phase: RunPhase, summary: &str) -> Vec<RuntimeEventKind> {
    (snapshot.phase != phase)
        .then_some(RuntimeEventKind::PhaseChanged {
            phase,
            summary: Some(summary.to_string()),
        })
        .into_iter()
        .collect()
}

fn task_state_change(
    task_id: &str,
    state: TaskState,
    lease: Option<LeaseRecord>,
    reason: Option<String>,
    retry_at: Option<DateTime<Utc>>,
    continuation_reason: Option<ContinuationReason>,
) -> RuntimeEventKind {
    RuntimeEventKind::TaskStateChanged {
        task_id: task_id.to_string(),
        state,
        lease,
        reason,
        retry_at,
        continuation_reason,
    }
}

fn dependencies_satisfied(snapshot: &RunSnapshot, task: &TaskNode) -> bool {
    task.deps.iter().all(|dep_id| {
        snapshot.tasks.get(dep_id).is_some_and(|dep| {
            matches!(
                dep.runtime.state,
                TaskState::Succeeded | TaskState::Superseded
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::events::RuntimeEventKind;
    use crate::runtime::types::{
        AutonomyLevel, LeaseRecord, RunBudgets, RunConstraints, RunSpec, TaskConstraints,
        TaskRuntime, TaskSpec, VerifySpec,
    };
    use chrono::Duration;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sample_snapshot() -> RunSnapshot {
        RunSnapshot {
            spec: RunSpec {
                run_id: "run-1".to_string(),
                objective: "ship runtime".to_string(),
                root_workspace: PathBuf::from("/tmp/pi"),
                policy_profile: "default".to_string(),
                model_profile: "default".to_string(),
                run_verify_command: Some("cargo test runtime".to_string()),
                run_verify_timeout_sec: Some(60),
                budgets: RunBudgets::default(),
                constraints: RunConstraints::default(),
                created_at: Utc::now(),
            },
            phase: RunPhase::Dispatching,
            plan_required: true,
            plan_accepted: true,
            plan: None,
            tasks: BTreeMap::new(),
            jobs: BTreeMap::new(),
            approvals: BTreeMap::new(),
            ready_queue: Default::default(),
            wake_at: None,
            dispatch: Default::default(),
            summary: Default::default(),
            version: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn sample_task(task_id: &str) -> TaskNode {
        TaskNode {
            spec: TaskSpec {
                task_id: task_id.to_string(),
                title: task_id.to_string(),
                objective: "do work".to_string(),
                parent_goal_trace_id: None,
                planned_touches: Vec::new(),
                input_snapshot: None,
                max_attempts: 1,
                enforce_symbol_drift_check: false,
                verify: VerifySpec {
                    command: "cargo test runtime".to_string(),
                    timeout_sec: 60,
                    acceptance_ids: Vec::new(),
                },
                autonomy: AutonomyLevel::Guarded,
                constraints: TaskConstraints::default(),
            },
            runtime: TaskRuntime::default(),
            deps: Vec::new(),
            children: Vec::new(),
            evidence_ids: Vec::new(),
        }
    }

    #[test]
    fn maintenance_promotes_blocked_tasks_when_dependencies_finish() {
        let mut snapshot = sample_snapshot();
        let mut dep = sample_task("task-a");
        dep.runtime.state = TaskState::Succeeded;
        let mut blocked = sample_task("task-b");
        blocked.runtime.state = TaskState::Blocked;
        blocked.deps = vec!["task-a".to_string()];
        snapshot.tasks.insert("task-a".to_string(), dep);
        snapshot.tasks.insert("task-b".to_string(), blocked);

        let events = maintenance_events(&snapshot, Utc::now());

        assert_eq!(
            events,
            vec![RuntimeEventKind::TaskStateChanged {
                task_id: "task-b".to_string(),
                state: TaskState::Ready,
                lease: None,
                reason: None,
                retry_at: None,
                continuation_reason: Some(ContinuationReason::TaskReady),
            }]
        );
    }

    #[test]
    fn maintenance_recovers_expired_leases() {
        let mut snapshot = sample_snapshot();
        let mut task = sample_task("task-a");
        task.runtime.state = TaskState::Leased;
        task.runtime.lease = Some(LeaseRecord {
            lease_id: "lease-1".to_string(),
            owner: "agent-1".to_string(),
            fence_token: 1,
            expires_at: Utc::now() - Duration::seconds(1),
        });
        snapshot.tasks.insert("task-a".to_string(), task);

        let events = maintenance_events(&snapshot, Utc::now());

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            RuntimeEventKind::TaskStateChanged {
                task_id,
                state: TaskState::Recoverable,
                continuation_reason: Some(ContinuationReason::WakeTimer),
                ..
            } if task_id == "task-a"
        ));
    }

    #[test]
    fn phase_events_schedule_wake_for_future_retry() {
        let mut snapshot = sample_snapshot();
        snapshot.ready_queue.clear();
        let mut task = sample_task("task-a");
        task.runtime.state = TaskState::Recoverable;
        task.runtime.retry_at = Some(Utc::now() + Duration::seconds(30));
        snapshot.tasks.insert("task-a".to_string(), task);

        let events = phase_and_wake_events(&snapshot, Utc::now());

        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            RuntimeEventKind::PhaseChanged {
                phase: RunPhase::Recovering,
                ..
            }
        ));
        assert!(matches!(events[1], RuntimeEventKind::WakeScheduled { .. }));
    }

    #[test]
    fn rebuild_ready_queue_respects_parallelism_and_touch_conflicts() {
        let mut snapshot = sample_snapshot();
        snapshot.spec.budgets.max_parallelism = 2;
        snapshot.dispatch.selected_tier = crate::orchestration::ExecutionTier::Wave;

        let mut task_a = sample_task("task-a");
        task_a.runtime.state = TaskState::Ready;
        task_a.spec.planned_touches = vec![PathBuf::from("src/a.rs")];

        let mut task_b = sample_task("task-b");
        task_b.runtime.state = TaskState::Ready;
        task_b.spec.planned_touches = vec![PathBuf::from("src/b.rs")];

        let mut task_c = sample_task("task-c");
        task_c.runtime.state = TaskState::Ready;
        task_c.spec.planned_touches = vec![PathBuf::from("src/a.rs")];

        snapshot.tasks.insert("task-a".to_string(), task_a);
        snapshot.tasks.insert("task-b".to_string(), task_b);
        snapshot.tasks.insert("task-c".to_string(), task_c);

        rebuild_ready_queue(&mut snapshot);

        assert_eq!(
            snapshot.ready_queue.iter().cloned().collect::<Vec<_>>(),
            vec!["task-a".to_string(), "task-b".to_string()]
        );
    }
}
