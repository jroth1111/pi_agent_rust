//! Runtime control-plane service contracts.
//!
//! These types freeze the architectural seam described in
//! `runtime-control-plane-unification/`: surfaces talk to typed contracts,
//! engines own business state, and cross-cutting planes absorb shared runtime
//! concerns such as inference, context, workspace, host execution, identity,
//! capability approval, and admission.

pub mod bootstrap;
pub mod boundary;
pub mod dto;
pub mod engine;
pub mod kernel;
pub mod plane;
pub mod runtime;

pub use bootstrap::{
    BootstrapPaths, BootstrapProfile, BootstrapRequest, InteractionMode, ResumeTarget,
    SurfaceCapabilities, SurfaceKind,
};
pub use boundary::{ContractBoundary, SurfaceBoundary, assert_surface_boundary};
pub use dto::{
    AdmissionControl, AdmissionDecision, AdmissionGrant, ApprovalChoice, ApprovalControl,
    ApprovalDecision, ApprovalPrompt, ContextBudget, ContextFreshness, ContextPack,
    ContextProvenance, DurabilityMode, InferenceReceipt, InterruptControl, InterruptReason,
    InterruptResult, ModelControl, ModelSelection, PersistenceSnapshot, PersistenceStoreKind,
    QueueControl, QueueDrainResult, QueueEnqueueResult, QueueKind, QueueMode, ResourceRequest,
    SessionControl, SessionIdentity, ThinkingLevel, WorkerLaunchEnvelope, WorkerRuntimeKind,
    WorkloadClass, WorkspaceSnapshot,
};
pub use engine::{
    AppendEvidenceRequest, ConversationContract, DispatchOptions, LeaseGrant, PersistenceContract,
    PrerequisiteTrigger, RunLifecycle, RunStatus, StateDigestResult, SubmitTaskRequest,
    SubmitTaskResult, TaskContract, TaskPrerequisite, TaskStateDigest, WaveStatus,
    WorkerRuntimeContract, WorkflowContract,
};
pub use kernel::{ApplicationKernel, ApplicationKernelBuilder};
pub use plane::{
    ActorIdentity, ActorType, AdmissionContract, AdmissionRecord, AdmissionState,
    CapabilityContract, CapabilityDecision, CapabilityRequest, ConflictInfo, ConflictType,
    ContextContract, ContextPackConfig, ContextPackUpdate, ContextSection, Credentials,
    ExecutionReceipt, ExecutionRequest, ExecutionResult, ExtensionLoadSpec, ExtensionPolicy,
    ExtensionRuntimeConfig, ExtensionRuntimeContract, FileContent, FileReadOptions,
    HostExecutionContract, IdentityContract, InferenceConfig, InferenceContract, InferenceSession,
    ModelInfo, Patch, PatchResult, RetrievalResult, RuntimeKind, RuntimeState, SecretHandle,
    SecretHandleRequest, SecretType, ToolDefinition, WorkspaceConfig, WorkspaceContract,
    WorkspaceStatus, WorktreeConfig, WorktreeInfo,
};
pub use runtime::{
    BootstrapOutcome, BootstrapWarning, KernelReadiness, RuntimeBootstrapContract, ServiceState,
};

/// Contract version for compatibility tracking.
pub const CONTRACT_VERSION: &str = "1.0.0";
