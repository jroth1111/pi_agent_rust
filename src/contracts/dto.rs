//! Surface-facing DTOs and control schemas.
//!
//! These are the shared wire and control types that surfaces can depend on
//! without reaching into engine internals.

use serde::{Deserialize, Serialize};

/// Model selection and thinking configuration for surface control.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelControl {
    pub model_id: String,
    pub provider: String,
    pub thinking_level: ThinkingLevel,
    pub thinking_budget_tokens: Option<u32>,
}

/// Model selection result from the model selector service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub control: ModelControl,
    pub is_fallback: bool,
    pub fallback_message: Option<String>,
}

/// Thinking level for reasoning-capable models.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
    XHigh,
}

/// Session identity for surface operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdentity {
    pub session_id: String,
    pub name: Option<String>,
    pub path: Option<String>,
}

/// Session lifecycle controls exposed to surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionControl {
    pub session_id: String,
    pub name: Option<String>,
    pub path: Option<String>,
    pub is_active: bool,
    pub autosave_enabled: bool,
    pub durability_mode: DurabilityMode,
}

/// Autosave durability mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DurabilityMode {
    Strict,
    #[default]
    Balanced,
    Throughput,
}

/// Queue delivery mode for steering/follow-up traffic.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// Message queue kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QueueKind {
    Steering,
    FollowUp,
}

/// Queue state surfaced to CLI/TUI/RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueControl {
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub pending_steering: usize,
    pub pending_follow_up: usize,
}

/// Result of enqueueing a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueEnqueueResult {
    pub seq: u64,
    pub queue: QueueKind,
}

/// Result of draining a queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueDrainResult {
    pub count: usize,
    pub seqs: Vec<u64>,
}

/// Interrupt control payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InterruptControl {
    pub reason: InterruptReason,
    pub hard_abort: bool,
}

/// Interrupt reason.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InterruptReason {
    UserCancel,
    SystemCancel,
    Timeout,
    ResourceLimit,
    DependencyFailed,
    PolicyViolation,
}

/// Interrupt result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InterruptResult {
    pub success: bool,
    pub reason: InterruptReason,
    pub cancelled_count: usize,
}

/// Choice offered in an approval prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalChoice {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Defer,
}

/// Approval prompt emitted by the capability plane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalPrompt {
    pub request_id: String,
    pub capability: String,
    pub description: String,
    pub actor: String,
    pub resource: Option<String>,
    pub choices: Vec<ApprovalChoice>,
    pub timeout_seconds: Option<u64>,
}

/// Approval decision returned by a surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecision {
    pub request_id: String,
    pub choice: ApprovalChoice,
    pub timestamp: i64,
}

/// Surface-level approval behavior controls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalControl {
    pub enabled: bool,
    pub default_timeout_seconds: u64,
    pub auto_approve_safe: bool,
}

/// Admission control request shared across surfaces and runtime planes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionControl {
    pub workload_class: WorkloadClass,
    pub requested: ResourceRequest,
    pub priority: u8,
}

/// High-level workload class used for admission.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkloadClass {
    Inference,
    HostExecution,
    WorkerLaunch,
    Verification,
    Compaction,
    WorkspaceHeavy,
}

/// Resource request shape for admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequest {
    pub memory_bytes: Option<u64>,
    pub cpu_percent: Option<u8>,
    pub concurrency: Option<usize>,
    pub timeout_seconds: Option<u64>,
}

/// Admission decision class.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionDecision {
    Admitted,
    Deferred,
    Denied,
}

/// Admission grant payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionGrant {
    pub decision: AdmissionDecision,
    pub grant_id: Option<String>,
    pub queue_position: Option<usize>,
    pub denial_reason: Option<String>,
}

/// Receipt from the inference plane for model execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InferenceReceipt {
    pub receipt_id: String,
    pub model_id: String,
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: Option<u64>,
    pub duration_ms: u64,
    pub success: bool,
    pub error: Option<String>,
}

/// Context pack emitted by the context plane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextPack {
    pub pack_id: String,
    pub provenance: ContextProvenance,
    pub freshness: ContextFreshness,
    pub budget: ContextBudget,
}

/// Provenance metadata for a context pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextProvenance {
    pub source: String,
    pub assembled_at: i64,
    pub session_id: Option<String>,
}

/// Freshness metadata for a context pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextFreshness {
    pub is_fresh: bool,
    pub age_seconds: u64,
    pub staleness_threshold_seconds: u64,
}

/// Budget metadata for a context pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextBudget {
    pub token_budget: u64,
    pub tokens_used: u64,
    pub exceeded: bool,
}

/// Snapshot of a workspace binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    pub snapshot_id: String,
    pub repo_root: String,
    pub worktree_path: String,
    pub branch: Option<String>,
    pub head_commit: Option<String>,
    pub is_clean: bool,
    pub patch_digest: Option<String>,
}

/// Snapshot of persistence-plane health and offsets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceSnapshot {
    pub store_kind: PersistenceStoreKind,
    pub is_healthy: bool,
    pub entry_count: usize,
    pub last_persisted_offset: u64,
    pub pending_mutations: usize,
}

/// Persistence store kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PersistenceStoreKind {
    Jsonl,
    Sqlite,
    V2Sidecar,
}

/// Worker launch envelope handed from the workflow engine to a worker runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerLaunchEnvelope {
    pub task_id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub launch_id: String,
    pub actor: String,
    pub requested_runtime: WorkerRuntimeKind,
    pub resolved_runtime: WorkerRuntimeKind,
    pub lease_id: String,
    pub fence_token: u64,
    pub workspace_snapshot_id: String,
    pub context_pack_id: String,
    pub admission_grant_id: String,
    pub capability_policy_id: String,
    pub verification_policy_id: String,
}

/// Worker runtime kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkerRuntimeKind {
    QuickJs,
    NativeRust,
    Wasm,
}
