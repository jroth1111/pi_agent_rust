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

impl std::str::FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" | "0" => Ok(Self::Off),
            "low" | "l" => Ok(Self::Low),
            "medium" | "med" | "m" => Ok(Self::Medium),
            "high" | "h" => Ok(Self::High),
            "xhigh" | "x-high" | "xh" => Ok(Self::XHigh),
            _ => Err(format!("invalid thinking level: {s}")),
        }
    }
}

impl std::fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::XHigh => write!(f, "xhigh"),
        }
    }
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

impl QueueMode {
    /// Convert from agent::QueueMode.
    pub const fn from_agent(mode: crate::agent::QueueMode) -> Self {
        match mode {
            crate::agent::QueueMode::All => Self::All,
            crate::agent::QueueMode::OneAtATime => Self::OneAtATime,
        }
    }

    /// Convert to agent::QueueMode.
    pub const fn to_agent(self) -> crate::agent::QueueMode {
        match self {
            Self::All => crate::agent::QueueMode::All,
            Self::OneAtATime => crate::agent::QueueMode::OneAtATime,
        }
    }
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
    /// The authoritative session event store (V2 segmented append log).
    EventStore,
}

/// Role tag for legacy store access after cutover.
///
/// After the authoritative event store cutover, JSONL/SQLite paths must only
/// be used under one of these explicitly-approved roles.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LegacyStoreRole {
    /// Import data from a legacy format into the event store.
    Import,
    /// Export data from the event store into a legacy format.
    Export,
    /// One-time migration from a legacy format to the event store.
    Migration,
    /// Offline inspection (read-only) of a legacy format.
    Inspection,
}

/// A persisted session event in the authoritative event store.
///
/// This wraps the existing `SessionEntry` types with event store metadata
/// (sequence number, timestamps, checksums) that the store manages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    /// Monotonically increasing sequence number within the session.
    pub seq: u64,
    /// Stable unique identifier for this event.
    pub event_id: String,
    /// Parent event ID for tree structure (null for root events).
    pub parent_event_id: Option<String>,
    /// The typed session entry payload.
    pub payload: SessionEventPayload,
    /// ISO 8601 timestamp when the event was appended.
    pub timestamp: String,
    /// SHA-256 checksum of the serialized payload for integrity.
    pub payload_checksum: String,
}

/// Payload types for session events.
///
/// This mirrors the existing `SessionEntry` variants but as a flat-tagged
/// union so the event store can persist them without coupling to the
/// in-memory `Session` representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventPayload {
    Message {
        role: String,
        content: serde_json::Value,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    ThinkingLevelChange {
        thinking_level: String,
    },
    Compaction {
        summary: String,
        #[serde(default)]
        compacted_entry_count: u64,
        #[serde(default)]
        original_message_count: u64,
        continuity: Option<serde_json::Value>,
    },
    BranchSummary {
        summary: String,
        from_leaf_id: Option<String>,
    },
    Label {
        label: String,
        target_entry_id: Option<String>,
    },
    SessionInfo {
        key: String,
        value: serde_json::Value,
    },
    /// Reliability/workflow entries stored as typed events.
    ReliabilityStateDigest {
        payload: serde_json::Value,
    },
    ReliabilityTaskCheckpoint {
        payload: serde_json::Value,
    },
    ReliabilityTaskCreated {
        payload: serde_json::Value,
    },
    ReliabilityTaskTransition {
        payload: serde_json::Value,
    },
    ReliabilityVerificationEvidence {
        payload: serde_json::Value,
    },
    ReliabilityCloseDecision {
        payload: serde_json::Value,
    },
    ReliabilityHumanBlockerRaised {
        payload: serde_json::Value,
    },
    ReliabilityHumanBlockerResolved {
        payload: serde_json::Value,
    },
    /// Extension-defined structured payload.
    Custom {
        name: String,
        payload: serde_json::Value,
    },
}

/// Rebuildable projection of session state derived from the event store.
///
/// Projections are *never* co-equal truth. They are derived from the
/// authoritative event store and can be fully rebuilt at any time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionProjection {
    /// The session identity this projection covers.
    pub session_id: String,
    /// Total number of events in the session.
    pub event_count: u64,
    /// The current leaf event ID (active position in the tree).
    pub leaf_event_id: Option<String>,
    /// Whether the tree is strictly linear (no branching).
    pub is_linear: bool,
    /// Total message count (user + assistant).
    pub message_count: u64,
    /// Most recent session name from SessionInfo entries.
    pub session_name: Option<String>,
    /// Current model and provider from the latest ModelChange event.
    pub current_model: Option<ModelControl>,
    /// Current thinking level from the latest ThinkingLevelChange event.
    pub current_thinking_level: Option<String>,
    /// The event store offset this projection was built from.
    pub built_from_offset: u64,
}

/// Result of validating a legacy session store for migration/import.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LegacyStoreValidation {
    pub role: LegacyStoreRole,
    pub source_kind: PersistenceStoreKind,
    pub entry_count: u64,
    pub is_valid: bool,
    pub errors: Vec<String>,
}

/// Parameters for importing a legacy session into the event store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LegacyImportRequest {
    pub source_path: String,
    pub source_kind: PersistenceStoreKind,
    pub role: LegacyStoreRole,
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
