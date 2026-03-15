use crate::runtime::events::{RuntimeEvent, RuntimeEventKind};
use crate::runtime::model_routing::{ModelRoute, ModelRouter, RouteRequest};
use crate::runtime::policy::{PolicyDecision, PolicyRequest, PolicyTarget, RuntimePolicy};
use crate::runtime::scheduler;
use crate::runtime::state_machine::{RuntimeStateMachine, RuntimeTransition, TransitionError};
use crate::runtime::types::{
    ApprovalCheckpoint, ApprovalId, ApprovalState, ContinuationReason, LeaseRecord, PlanArtifact,
    RunPhase, RunSnapshot, RunSpec, TaskNode, TaskState,
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
    AcceptPlan,
    SetPhase {
        phase: RunPhase,
        summary: Option<String>,
    },
    MarkTaskState {
        task_id: String,
        state: TaskState,
        lease: Option<LeaseRecord>,
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
        let now = Utc::now();
        let run_id = machine.snapshot().spec.run_id.clone();
        let mut events = scheduler::maintenance_events(machine.snapshot(), now)
            .into_iter()
            .map(|kind| RuntimeEvent::new(run_id.clone(), kind))
            .collect::<Vec<_>>();
        let mut transitions = apply_events(machine, &events)?;

        let phase_events = scheduler::phase_and_wake_events(machine.snapshot(), now)
            .into_iter()
            .map(|kind| RuntimeEvent::new(run_id.clone(), kind))
            .collect::<Vec<_>>();
        transitions.extend(apply_events(machine, &phase_events)?);
        events.extend(phase_events);
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

                let mut snapshot = RunSnapshot::new(spec.clone());
                snapshot.plan_required = true;
                let mut machine = RuntimeStateMachine::new(snapshot);
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
                    RuntimeEventKind::PlanMaterialized { plan, tasks },
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
                    RuntimeCommand::AcceptPlan => {
                        RuntimeEvent::new(run_id, RuntimeEventKind::PlanAccepted)
                    }
                    RuntimeCommand::MarkTaskState {
                        task_id,
                        state,
                        lease,
                        reason,
                        retry_at,
                        continuation_reason,
                    } => RuntimeEvent::new(
                        run_id,
                        RuntimeEventKind::TaskStateChanged {
                            task_id,
                            state,
                            lease,
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
            parent_goal_trace_id: None,
            planned_touches: vec![PathBuf::from("src/runtime/controller.rs")],
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
        assert_eq!(output.snapshot.phase, RunPhase::Planning);
        assert_eq!(output.events.len(), 3);
        assert_eq!(output.snapshot.tasks.len(), 1);
        assert!(!output.snapshot.plan_accepted);
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
        controller
            .handle(RuntimeCommand::AcceptPlan)
            .expect("accept plan");
        let output = controller
            .handle(RuntimeCommand::MarkTaskState {
                task_id: "task-1".to_string(),
                state: TaskState::Executing,
                lease: None,
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

    #[test]
    fn bootstrap_can_require_manual_dispatch_after_planning() {
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

        assert_eq!(output.snapshot.phase, RunPhase::Planning);
        assert!(output.snapshot.plan_required);
        assert!(!output.snapshot.plan_accepted);
    }

    #[test]
    fn tick_schedules_wake_for_future_recoverable_retry() {
        let mut snapshot = RunSnapshot::new(sample_spec());
        snapshot.phase = RunPhase::Dispatching;
        snapshot.plan_required = true;
        snapshot.plan_accepted = true;
        let mut task = sample_task();
        task.runtime.state = TaskState::Recoverable;
        task.runtime.retry_at = Some(Utc::now() + chrono::Duration::seconds(10));
        snapshot.tasks.insert(task.spec.task_id.clone(), task);
        let router = PhaseModelRouter::new(sample_route());
        let policy = PolicySet::new();
        let mut controller = RuntimeController::from_snapshot(snapshot, router, policy);

        let output = controller.tick().expect("tick");

        assert_eq!(output.snapshot.phase, RunPhase::Recovering);
        assert!(output.snapshot.wake_at.is_some());
        assert_eq!(output.events.len(), 2);
    }
}
