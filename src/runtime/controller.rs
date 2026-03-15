use crate::runtime::events::{RuntimeEvent, RuntimeEventKind};
use crate::runtime::model_routing::{ModelRoute, ModelRouter, RouteRequest};
use crate::runtime::policy::{PolicyDecision, PolicyRequest, PolicyTarget, RuntimePolicy};
use crate::runtime::state_machine::{RuntimeStateMachine, RuntimeTransition, TransitionError};
use crate::runtime::types::{
    ApprovalCheckpoint, ApprovalId, ApprovalState, ContinuationReason, PlanArtifact, RunPhase,
    RunSnapshot, RunSpec, TaskNode, TaskState,
};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum RuntimeCommand {
    BootstrapRun {
        spec: RunSpec,
        plan: PlanArtifact,
        tasks: Vec<TaskNode>,
    },
    SetPhase {
        phase: RunPhase,
        summary: Option<String>,
    },
    MarkTaskState {
        task_id: String,
        state: TaskState,
        reason: Option<String>,
        retry_at: Option<DateTime<Utc>>,
        continuation_reason: Option<ContinuationReason>,
    },
    RequestApproval {
        checkpoint: ApprovalCheckpoint,
    },
    ResolveApproval {
        approval_id: ApprovalId,
        approved: bool,
    },
    CompleteRun,
    FailRun {
        reason: String,
    },
    CancelRun {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct RoutedPhase {
    pub phase: RunPhase,
    pub route: ModelRoute,
}

#[derive(Debug, Clone)]
pub struct ControllerOutput {
    pub snapshot: RunSnapshot,
    pub events: Vec<RuntimeEvent>,
    pub transitions: Vec<RuntimeTransition>,
    pub routed_phases: Vec<RoutedPhase>,
    pub policy_decisions: Vec<PolicyDecision>,
}

#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("policy denied bootstrap: {0}")]
    PolicyDenied(String),
    #[error("runtime controller must be bootstrapped before handling commands")]
    Uninitialized,
}

pub struct RuntimeController<R, P> {
    state_machine: Option<RuntimeStateMachine>,
    router: R,
    policy: P,
}

impl<R, P> RuntimeController<R, P>
where
    R: ModelRouter,
    P: RuntimePolicy,
{
    pub const fn new(router: R, policy: P) -> Self {
        Self {
            state_machine: None,
            router,
            policy,
        }
    }

    pub const fn from_snapshot(snapshot: RunSnapshot, router: R, policy: P) -> Self {
        Self {
            state_machine: Some(RuntimeStateMachine::new(snapshot)),
            router,
            policy,
        }
    }

    pub const fn state_machine(&self) -> Option<&RuntimeStateMachine> {
        self.state_machine.as_ref()
    }

    pub fn into_parts(self) -> (Option<RuntimeStateMachine>, R, P) {
        (self.state_machine, self.router, self.policy)
    }

    pub fn tick(&mut self) -> Result<ControllerOutput, ControllerError> {
        let machine = self
            .state_machine
            .as_mut()
            .ok_or(ControllerError::Uninitialized)?;
        let snapshot = machine.snapshot().clone();
        let events = derive_tick_event(&snapshot)
            .map(|kind| RuntimeEvent::new(snapshot.spec.run_id.clone(), kind))
            .into_iter()
            .collect::<Vec<_>>();
        let transitions = apply_events(machine, &events)?;
        let snapshot = machine.snapshot().clone();
        let routed_phases = routed_phase_for(&self.router, snapshot.phase);
        Ok(ControllerOutput {
            snapshot,
            events,
            transitions,
            routed_phases,
            policy_decisions: Vec::new(),
        })
    }

    pub fn handle(&mut self, command: RuntimeCommand) -> Result<ControllerOutput, ControllerError> {
        let mut routed_phases = Vec::new();
        let mut policy_decisions = Vec::new();
        let mut events = Vec::new();

        match command {
            RuntimeCommand::BootstrapRun { spec, plan, tasks } => {
                let mut request = PolicyRequest::new(PolicyTarget::Run {
                    run_id: spec.run_id.clone(),
                    objective: spec.objective.clone(),
                });
                request.planned_touches = tasks
                    .iter()
                    .flat_map(|task| {
                        task.spec
                            .planned_touches
                            .iter()
                            .map(|path| path.display().to_string())
                    })
                    .collect();
                let decision = self.policy.evaluate(&request);
                if decision.verdict.is_denied() {
                    let reason = decision
                        .reasons
                        .first()
                        .map(|reason| reason.message.clone())
                        .unwrap_or_else(|| "runtime bootstrap denied".to_string());
                    return Err(ControllerError::PolicyDenied(reason));
                }
                policy_decisions.push(decision);

                let mut machine = RuntimeStateMachine::new(RunSnapshot::new(spec.clone()));
                let run_id = spec.run_id.clone();
                routed_phases = routed_phase_for(&self.router, RunPhase::Planning);

                events.push(RuntimeEvent::new(
                    run_id.clone(),
                    RuntimeEventKind::RunCreated,
                ));
                events.push(RuntimeEvent::new(
                    run_id.clone(),
                    RuntimeEventKind::PlanningStarted,
                ));
                events.push(RuntimeEvent::new(
                    run_id,
                    RuntimeEventKind::PlanAccepted { plan, tasks },
                ));

                let transitions = apply_events(&mut machine, &events)?;
                let snapshot = machine.snapshot().clone();
                self.state_machine = Some(machine);
                Ok(ControllerOutput {
                    snapshot,
                    events,
                    transitions,
                    routed_phases,
                    policy_decisions,
                })
            }
            command => {
                let machine = self
                    .state_machine
                    .as_mut()
                    .ok_or(ControllerError::Uninitialized)?;
                let run_id = machine.snapshot().spec.run_id.clone();
                let event = match command {
                    RuntimeCommand::SetPhase { phase, summary } => {
                        RuntimeEvent::new(run_id, RuntimeEventKind::PhaseChanged { phase, summary })
                    }
                    RuntimeCommand::MarkTaskState {
                        task_id,
                        state,
                        reason,
                        retry_at,
                        continuation_reason,
                    } => RuntimeEvent::new(
                        run_id,
                        RuntimeEventKind::TaskStateChanged {
                            task_id,
                            state,
                            reason,
                            retry_at,
                            continuation_reason,
                        },
                    ),
                    RuntimeCommand::RequestApproval { checkpoint } => RuntimeEvent::new(
                        run_id,
                        RuntimeEventKind::ApprovalRequested { checkpoint },
                    ),
                    RuntimeCommand::ResolveApproval {
                        approval_id,
                        approved,
                    } => RuntimeEvent::new(
                        run_id,
                        RuntimeEventKind::ApprovalResolved {
                            approval_id,
                            state: if approved {
                                ApprovalState::Approved
                            } else {
                                ApprovalState::Denied
                            },
                        },
                    ),
                    RuntimeCommand::CompleteRun => {
                        RuntimeEvent::new(run_id, RuntimeEventKind::RunCompleted)
                    }
                    RuntimeCommand::FailRun { reason } => {
                        RuntimeEvent::new(run_id, RuntimeEventKind::RunFailed { reason })
                    }
                    RuntimeCommand::CancelRun { reason } => {
                        RuntimeEvent::new(run_id, RuntimeEventKind::RunCanceled { reason })
                    }
                    RuntimeCommand::BootstrapRun { .. } => unreachable!("handled above"),
                };
                events.push(event);
                let transitions = apply_events(machine, &events)?;
                let snapshot = machine.snapshot().clone();
                Ok(ControllerOutput {
                    snapshot,
                    events,
                    transitions,
                    routed_phases,
                    policy_decisions,
                })
            }
        }
    }
}

fn apply_events(
    machine: &mut RuntimeStateMachine,
    events: &[RuntimeEvent],
) -> Result<Vec<RuntimeTransition>, TransitionError> {
    let mut transitions = Vec::with_capacity(events.len());
    for event in events {
        transitions.push(machine.apply(event.clone())?.clone());
    }
    Ok(transitions)
}

fn routed_phase_for<R>(router: &R, phase: RunPhase) -> Vec<RoutedPhase>
where
    R: ModelRouter,
{
    router
        .route(&RouteRequest {
            phase,
            task_id: None,
        })
        .map(|route| vec![RoutedPhase { phase, route }])
        .unwrap_or_default()
}

fn derive_tick_event(snapshot: &RunSnapshot) -> Option<RuntimeEventKind> {
    if snapshot.phase.is_terminal() || snapshot.tasks.is_empty() {
        return None;
    }

    let tasks = snapshot.tasks.values().collect::<Vec<_>>();
    let approvals_pending = snapshot
        .approvals
        .values()
        .any(|approval| approval.state == ApprovalState::Pending);
    let awaiting_human = approvals_pending
        || tasks
            .iter()
            .any(|task| task.runtime.state == TaskState::AwaitingHuman);
    let recoverable = tasks
        .iter()
        .any(|task| task.runtime.state == TaskState::Recoverable);
    let verifying = tasks
        .iter()
        .any(|task| task.runtime.state == TaskState::Verifying);
    let active = tasks
        .iter()
        .any(|task| matches!(task.runtime.state, TaskState::Leased | TaskState::Executing));
    let all_terminal = tasks.iter().all(|task| task.runtime.state.is_terminal());
    let all_succeeded = tasks
        .iter()
        .all(|task| task.runtime.state == TaskState::Succeeded);

    if all_terminal {
        return if all_succeeded {
            Some(RuntimeEventKind::RunCompleted)
        } else {
            Some(RuntimeEventKind::RunFailed {
                reason: "one or more tasks reached a failed terminal state".to_string(),
            })
        };
    }

    let (phase, summary) = if awaiting_human {
        (
            RunPhase::AwaitingHuman,
            Some("await human approval".to_string()),
        )
    } else if recoverable {
        (
            RunPhase::Recovering,
            Some("retry recoverable work".to_string()),
        )
    } else if verifying {
        (
            RunPhase::Verifying,
            Some("verify current outputs".to_string()),
        )
    } else if active {
        (RunPhase::Running, Some("continue active work".to_string()))
    } else {
        return None;
    };

    (snapshot.phase != phase).then_some(RuntimeEventKind::PhaseChanged { phase, summary })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model_routing::{ModelRoute, PhaseModelRouter};
    use crate::runtime::policy::PolicySet;
    use crate::runtime::types::{
        AutonomyLevel, ModelProfile, ModelSelector, RunBudgets, RunConstraints, TaskConstraints,
        TaskSpec, VerifySpec,
    };
    use std::path::PathBuf;

    fn sample_route() -> ModelRoute {
        ModelRoute::from_profile(&ModelProfile {
            planner: ModelSelector {
                provider: "anthropic".to_string(),
                model: "planner".to_string(),
                thinking_level: Some("high".to_string()),
            },
            executor: ModelSelector {
                provider: "openai".to_string(),
                model: "executor".to_string(),
                thinking_level: None,
            },
            verifier: ModelSelector {
                provider: "openai".to_string(),
                model: "verifier".to_string(),
                thinking_level: None,
            },
            summarizer: ModelSelector {
                provider: "openai".to_string(),
                model: "summary".to_string(),
                thinking_level: None,
            },
            background: ModelSelector {
                provider: "openai".to_string(),
                model: "background".to_string(),
                thinking_level: None,
            },
        })
    }

    fn sample_spec() -> RunSpec {
        RunSpec {
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
        }
    }

    fn sample_plan() -> PlanArtifact {
        PlanArtifact {
            plan_id: "plan-1".to_string(),
            digest: "digest".to_string(),
            objective: "ship runtime".to_string(),
            task_drafts: vec!["task-1".to_string()],
            touched_paths: vec![PathBuf::from("src/runtime/controller.rs")],
            test_strategy: vec!["cargo test runtime".to_string()],
            evidence_refs: Vec::new(),
            produced_at: Utc::now(),
        }
    }

    fn sample_task() -> TaskNode {
        TaskNode::new(TaskSpec {
            task_id: "task-1".to_string(),
            title: "task".to_string(),
            objective: "do work".to_string(),
            planned_touches: vec![PathBuf::from("src/runtime/controller.rs")],
            verify: VerifySpec {
                command: "cargo test runtime".to_string(),
                timeout_sec: 60,
                acceptance_ids: Vec::new(),
            },
            autonomy: AutonomyLevel::Guarded,
            constraints: TaskConstraints::default(),
        })
    }

    #[test]
    fn bootstrap_run_builds_snapshot_and_events() {
        let router = PhaseModelRouter::new(sample_route());
        let policy = PolicySet::new();
        let mut controller = RuntimeController::new(router, policy);
        let output = controller
            .handle(RuntimeCommand::BootstrapRun {
                spec: sample_spec(),
                plan: sample_plan(),
                tasks: vec![sample_task()],
            })
            .expect("bootstrap");
        assert_eq!(output.snapshot.phase, RunPhase::Dispatching);
        assert_eq!(output.events.len(), 3);
        assert_eq!(output.snapshot.tasks.len(), 1);
    }

    #[test]
    fn mark_task_state_updates_bootstrapped_controller() {
        let router = PhaseModelRouter::new(sample_route());
        let policy = PolicySet::new();
        let mut controller = RuntimeController::new(router, policy);
        controller
            .handle(RuntimeCommand::BootstrapRun {
                spec: sample_spec(),
                plan: sample_plan(),
                tasks: vec![sample_task()],
            })
            .expect("bootstrap");
        let output = controller
            .handle(RuntimeCommand::MarkTaskState {
                task_id: "task-1".to_string(),
                state: TaskState::Executing,
                reason: None,
                retry_at: None,
                continuation_reason: Some(ContinuationReason::PlanExecution),
            })
            .expect("update");
        assert_eq!(
            output.snapshot.tasks["task-1"].runtime.state,
            TaskState::Executing
        );
    }

    #[test]
    fn tick_marks_completed_when_all_tasks_succeed() {
        let mut snapshot = RunSnapshot::new(sample_spec());
        snapshot.phase = RunPhase::Running;
        let mut task = sample_task();
        task.runtime.state = TaskState::Succeeded;
        snapshot.tasks.insert(task.spec.task_id.clone(), task);
        let router = PhaseModelRouter::new(sample_route());
        let policy = PolicySet::new();
        let mut controller = RuntimeController::from_snapshot(snapshot, router, policy);

        let output = controller.tick().expect("tick");

        assert_eq!(output.snapshot.phase, RunPhase::Completed);
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].label(), "run_completed");
    }

    #[test]
    fn tick_moves_running_snapshot_to_awaiting_human() {
        let mut snapshot = RunSnapshot::new(sample_spec());
        snapshot.phase = RunPhase::Running;
        let mut task = sample_task();
        task.runtime.state = TaskState::AwaitingHuman;
        snapshot.tasks.insert(task.spec.task_id.clone(), task);
        let router = PhaseModelRouter::new(sample_route());
        let policy = PolicySet::new();
        let mut controller = RuntimeController::from_snapshot(snapshot, router, policy);

        let output = controller.tick().expect("tick");

        assert_eq!(output.snapshot.phase, RunPhase::AwaitingHuman);
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].label(), "phase_changed");
    }
}
