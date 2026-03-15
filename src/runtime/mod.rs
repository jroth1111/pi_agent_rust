//! Canonical runtime control plane for durable multi-step runs.
//!
//! This subsystem is the long-term source of truth for run state, event
//! history, model routing, and policy decisions. Existing orchestration paths
//! can migrate into it incrementally.

pub mod controller;
pub mod events;
pub mod model_routing;
pub mod policy;
pub mod scheduler;
pub mod state_machine;
pub mod store;
pub mod types;

pub use controller::{ControllerError, ControllerOutput, RuntimeCommand, RuntimeController};
pub use events::{RuntimeEvent, RuntimeEventKind};
pub use model_routing::{
    ModelRoute, ModelRouter, PhaseModelRouter, RouteError, RouteRequest, RuntimeModelRef,
};
pub use policy::{
    PolicyDecision, PolicyReason, PolicyRequest, PolicySet, PolicyTarget, PolicyVerdict,
    RuntimePolicy,
};
pub use state_machine::{RuntimeStateMachine, RuntimeTransition, TransitionError};
pub use store::RuntimeStore;
pub use types::{
    ApprovalCheckpoint, ApprovalState, ArtifactRef, AutonomyLevel, ContinuationReason,
    ExecutionTier, FailureRecord, JobId, JobKind, JobRecord, JobState, LeaseRecord, ModelProfile,
    ModelSelector, PlanArtifact, PlanId, RunBudgets, RunConstraints, RunDispatchState, RunId,
    RunLifecycle, RunPhase, RunSnapshot, RunSpec, RunStatus, RunSummary, RunVerifyScopeKind,
    RunVerifyStatus, SubrunPlan, TaskId, TaskNode, TaskReport, TaskRuntime, TaskSpec, TaskState,
    VerifySpec, WaveStatus,
};
