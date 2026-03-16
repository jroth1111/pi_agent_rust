//! Canonical runtime control plane for durable multi-step runs.
//!
//! This subsystem is the long-term source of truth for run state, event
//! history, model routing, and policy decisions. Existing orchestration paths
//! can migrate into it incrementally.

pub mod controller;
pub mod dispatch;
pub mod events;
pub mod execution;
pub mod model_routing;
pub mod policy;
pub mod reliability;
pub mod role_prompt;
pub mod scheduler;
pub mod service;
pub mod state_machine;
pub mod store;
pub mod types;
pub mod verification;

pub use controller::{ControllerError, ControllerOutput, RuntimeCommand, RuntimeController};
pub use dispatch::{
    DispatchRunHost, MAX_AUTOMATED_RECOVERABLE_WAIT, ORCHESTRATION_ROLLBACK_RETRY_DELAY,
    apply_runtime_scheduler_lifecycle, build_runtime_task_report, cancel_live_run_tasks,
    dispatch_run_until_quiescent, dispatch_run_wave, next_recoverable_retry_delay,
    refresh_live_run_from_reliability, refresh_run_from_reliability, run_has_live_tasks,
    run_requires_plan_acceptance, sync_runtime_snapshot_from_reliability,
    sync_runtime_task_from_reliability,
};
pub use events::{RuntimeEvent, RuntimeEventKind};
pub use execution::{
    AppendEvidenceRequest, BlockerReport, DispatchGrant, RuntimeExecutionState, SubmitTaskRequest,
    SubmitTaskResponse, TaskContract, TaskPrerequisite,
};
pub use model_routing::{
    ModelRoute, ModelRouter, PhaseModelRouter, RouteError, RouteRequest, RuntimeModelRef,
};
pub use policy::{
    PolicyDecision, PolicyReason, PolicyRequest, PolicySet, PolicyTarget, PolicyVerdict,
    RuntimePolicy,
};
pub use reliability::*;
pub use role_prompt::{SessionRole, build_role_system_prompt};
pub use service::{
    RuntimeServiceHost, RuntimeStartRunRequest, accept_plan, accept_run_plan, append_evidence,
    bootstrap_run, build_runtime_plan_artifact, build_runtime_task_nodes, cancel_run, dispatch_run,
    ensure_run_id_available, next_run_id, resolve_blocker, resume_run, select_execution_tier,
    start_run, submit_task, validate_start_run_request,
};
pub use state_machine::{RuntimeStateMachine, RuntimeTransition, TransitionError};
pub use store::RuntimeStore;
pub use types::{
    ApprovalCheckpoint, ApprovalState, ArtifactRef, AutonomyLevel, ContinuationReason,
    ExecutionTier, FailureRecord, JobId, JobKind, JobRecord, JobState, LeaseRecord, ModelProfile,
    ModelSelector, PlanArtifact, PlanId, RunBudgets, RunConstraints, RunDispatchState, RunId,
    RunPhase, RunSnapshot, RunSpec, RunSummary, RunVerifyScopeKind, RunVerifyStatus, SubrunPlan,
    TaskId, TaskNode, TaskReport, TaskRuntime, TaskSpec, TaskState, VerifySpec, WaveStatus,
};
pub use verification::{
    CompletedRunVerifyScope, apply_run_verify_lifecycle, completed_run_verify_scope,
    completed_scope_from_run_verify, execute_run_verification, should_skip_run_verify,
};
