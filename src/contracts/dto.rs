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
        /// Typed compaction continuity state. When present, replaces the
        /// legacy ad hoc `continuity: Option<Value>` blob with a structured
        /// record that can be deterministically replayed on resume.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continuity: Option<CompactionContinuity>,
    },
    BranchSummary {
        summary: String,
        from_leaf_id: Option<String>,
    },
    /// Records the activation of a single skill during the session.
    SkillActivation {
        skill_name: String,
        source: SkillSource,
        file_path: String,
        #[serde(default)]
        disable_model_invocation: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Records the complete active skill set at a point in session time.
    /// Emitted when skills are loaded or when the skill set changes.
    SkillActivationSnapshot {
        #[serde(default)]
        skills_disabled: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skills: Vec<SkillActivation>,
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

// ============================================================================
// Continuity state: typed durable state that survives save, resume, fork,
// and compaction without depending on prompt luck or ad hoc blobs.
// ============================================================================

/// Typed compaction continuity state persisted alongside compaction events.
///
/// This replaces the previous ad hoc `Option<serde_json::Value>` continuity
/// field. The typed structure ensures that compaction context can be
/// deterministically reconstructed on resume without relying on narrative
/// summary text alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionContinuity {
    /// Entry ID of the first entry retained after compaction.
    /// Entries before this boundary were summarized and removed.
    pub first_kept_entry_id: String,
    /// The event ID of the leaf at the time of compaction.
    /// Preserves branch identity across compaction boundaries.
    pub compaction_leaf_event_id: String,
    /// Total number of entries that existed before compaction.
    pub pre_compaction_entry_count: u64,
    /// Total message count before compaction.
    pub pre_compaction_message_count: u64,
    /// Number of entries removed by this compaction.
    pub compacted_entry_count: u64,
    /// Cumulative file tracking state carried forward for next compaction.
    #[serde(default, skip_serializing_if = "CompactionFileTracking::is_empty")]
    pub file_tracking: CompactionFileTracking,
}

/// Cumulative file tracking state persisted through compaction boundaries.
///
/// This ensures that progressive file tracking (read/modified files) survives
/// compaction and resume, enabling the next compaction to make correct
/// decisions based on the full history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFileTracking {
    /// Files read across the session lifetime (including pre-compaction).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_files: Vec<String>,
    /// Files modified across the session lifetime (including pre-compaction).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modified_files: Vec<String>,
}

impl CompactionFileTracking {
    /// Returns true if both file lists are empty.
    pub fn is_empty(&self) -> bool {
        self.read_files.is_empty() && self.modified_files.is_empty()
    }
}

/// Typed branch continuity state that survives save, resume, and fork.
///
/// The event store's parent_id links already provide the tree structure,
/// but this typed struct captures the *semantic* branch identity needed
/// for correct reconstruction without re-walking the entire event tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BranchContinuityState {
    /// The current leaf event ID (active position in the tree).
    pub leaf_event_id: String,
    /// The root event ID (first event in the session).
    pub root_event_id: Option<String>,
    /// Whether the tree is strictly linear (no branching).
    pub is_linear: bool,
    /// All known branch head event IDs (entries that have children but
    /// whose parent has multiple children — i.e., fork points).
    /// Empty when the session is linear.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branch_heads: Vec<String>,
    /// Event IDs of fork points where the tree diverged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fork_points: Vec<String>,
}

/// A single skill activation record persisted as a session event.
///
/// When a skill is loaded during a session, a `SkillActivation` event is
/// appended to the event store. This ensures that the active skill set
/// survives compaction and resume as canonical typed state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillActivation {
    /// Unique name of the skill (matches the skill file/directory name).
    pub skill_name: String,
    /// Source category where the skill was discovered.
    pub source: SkillSource,
    /// File path where the skill was loaded from.
    pub file_path: String,
    /// Whether the skill disables model invocation.
    #[serde(default)]
    pub disable_model_invocation: bool,
    /// Description from the skill's frontmatter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Source category for a skill activation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    /// Loaded from the user's agent directory (~/.pi/skills).
    User,
    /// Loaded from the project's .pi/skills directory.
    Project,
    /// Loaded from an explicit --skill path argument.
    Path,
}

/// The complete active skill set at a point in session time.
///
/// Emitted as a `SkillActivationSnapshot` event when the skill set changes,
/// and reconstructed from the event store on resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillActivationSet {
    /// All currently active skill activations.
    pub skills: Vec<SkillActivation>,
    /// Whether the `--no-skills` flag was set (suppresses all skill loading).
    #[serde(default)]
    pub skills_disabled: bool,
}

/// Result of validating skill activations on session resume.
///
/// When a session is resumed and the event store contains skill activations,
/// each referenced skill is validated against the currently available skills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillActivationValidation {
    /// Skills that were found and are still available.
    pub found_skills: Vec<String>,
    /// Skills that were in the activation set but are no longer available.
    /// This triggers fail-closed behavior — the session cannot be used for
    /// model invocation until the issue is resolved.
    pub missing_skills: Vec<MissingSkill>,
    /// Whether validation passed (all skills found, or no skills were active).
    pub is_valid: bool,
}

/// A skill that was activated in the session but is missing on resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MissingSkill {
    /// The skill name that was activated.
    pub skill_name: String,
    /// The file path the skill was loaded from (may no longer exist).
    pub expected_file_path: String,
    /// The source category of the missing skill.
    pub source: SkillSource,
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
    /// Typed branch continuity state derived from the event tree.
    /// None when the session has no events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_continuity: Option<BranchContinuityState>,
    /// The most recent compaction continuity state from the latest compaction
    /// event. None when no compaction has occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_continuity: Option<CompactionContinuity>,
    /// The active skill set derived from SkillActivation and
    /// SkillActivationSnapshot events. None when no skills were activated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_activations: Option<SkillActivationSet>,
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
