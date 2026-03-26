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

// ============================================================================
// Session integrity (VAL-SESS-007): typed integrity check results that are
// enforced before the store is considered healthy for reads or writes.
// ============================================================================

/// Severity level for an integrity violation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum IntegritySeverity {
    /// Non-critical: the session can still be served with a warning.
    Warning,
    /// Critical: the session must not be served until repaired.
    Critical,
}

/// A single integrity violation detected during session validation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityViolation {
    /// Machine-readable invariant ID (e.g., "INV-001").
    pub invariant_id: String,
    /// Human-readable description of the violation.
    pub description: String,
    /// Severity level.
    pub severity: IntegritySeverity,
    /// The entry ID(s) involved in the violation, if applicable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_ids: Vec<String>,
}

/// Outcome of an integrity check — either clean or contains violations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IntegrityOutcome {
    /// All invariants passed.
    Clean,
    /// One or more violations detected.
    Violations(Vec<IntegrityViolation>),
}

impl IntegrityOutcome {
    /// Returns true if all invariants passed.
    pub const fn is_clean(&self) -> bool {
        matches!(self, IntegrityOutcome::Clean)
    }

    /// Returns true if any critical violation exists.
    pub fn has_critical(&self) -> bool {
        matches!(
            self,
            IntegrityOutcome::Violations(violations) if violations.iter().any(|v| v.severity == IntegritySeverity::Critical)
        )
    }

    /// Returns the list of violations, or an empty vec if clean.
    pub fn violations(&self) -> Vec<IntegrityViolation> {
        match self {
            IntegrityOutcome::Clean => Vec::new(),
            IntegrityOutcome::Violations(v) => v.clone(),
        }
    }
}

/// Full integrity report for a session, produced before the store is
/// considered healthy for reads or writes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionIntegrityReport {
    /// The session ID this report covers.
    pub session_id: String,
    /// The overall outcome.
    pub outcome: IntegrityOutcome,
    /// Total number of entries checked.
    pub entry_count: usize,
    /// Whether parent-link closure holds (INV-001).
    pub parent_link_closure: bool,
    /// Whether entry IDs are unique (INV-002 variant).
    pub unique_entry_ids: bool,
    /// Whether branch-head indexing is consistent (INV-006 variant).
    pub branch_head_consistency: bool,
    /// ISO 8601 timestamp when the check was performed.
    pub checked_at: String,
}

// ============================================================================
// Hydration fidelity (VAL-SESS-005): typed state that prevents silent data
// loss during partial hydration and fast resume.
// ============================================================================

/// Hydration mode describing how a session was loaded into memory.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HydrationMode {
    /// All entries loaded — no fidelity risk.
    Full,
    /// Only the active path (root to leaf) was loaded.
    /// Dormant (non-active) branches are NOT in memory.
    ActivePath,
    /// Only the most recent N entries were loaded.
    /// Older entries and dormant branches are NOT in memory.
    Tail(u64),
}

impl HydrationMode {
    /// Returns true if this hydration mode loaded all entries.
    pub const fn is_full(&self) -> bool {
        matches!(self, HydrationMode::Full)
    }

    /// Returns true if this is a partial hydration that may be missing
    /// dormant branches or older entries.
    pub const fn is_partial(&self) -> bool {
        !self.is_full()
    }
}

/// Durable hydration state that is persisted and restored across restarts.
///
/// This ensures that after a fast resume with partial hydration, the system
/// knows exactly what subset of the session is in memory and can enforce
/// safeguards before any save that could silently discard dormant state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HydrationState {
    /// The hydration mode used when the session was loaded.
    pub mode: HydrationMode,
    /// Total number of entries in the authoritative store (not just loaded).
    pub total_entries: u64,
    /// Number of entries currently loaded in memory.
    pub loaded_entries: usize,
    /// The event store sequence number up to which entries are loaded.
    /// Entries after this seq are NOT loaded.
    pub loaded_up_to_seq: u64,
    /// The total event store sequence (last event seq).
    pub total_seq: u64,
    /// Whether a full rehydration has been requested but not yet completed.
    pub rehydration_pending: bool,
}

impl HydrationState {
    /// Create a full-hydration state (no fidelity risk).
    pub const fn full(entry_count: usize, seq: u64) -> Self {
        let entry_count = entry_count as u64;
        Self {
            mode: HydrationMode::Full,
            total_entries: entry_count,
            loaded_entries: entry_count as usize,
            loaded_up_to_seq: seq,
            total_seq: seq,
            rehydration_pending: false,
        }
    }

    /// Create a partial-hydration state with fidelity risk.
    pub const fn partial(
        mode: HydrationMode,
        total_entries: u64,
        loaded_entries: usize,
        loaded_up_to_seq: u64,
        total_seq: u64,
    ) -> Self {
        Self {
            mode,
            total_entries,
            loaded_entries,
            loaded_up_to_seq,
            total_seq,
            rehydration_pending: false,
        }
    }

    /// Returns true if all entries are loaded (no fidelity risk).
    pub const fn is_complete(&self) -> bool {
        self.loaded_entries as u64 >= self.total_entries
    }

    /// Returns true if this state represents a partial hydration.
    pub const fn has_fidelity_risk(&self) -> bool {
        self.mode.is_partial()
    }
}

// ============================================================================
// Durable autosave backlog (VAL-SESS-010): typed state that survives restart
// and makes autosave lag, flush outcomes, and retry state inspectable.
// ============================================================================

/// Durable autosave backlog state persisted alongside session metadata.
///
/// Unlike `AutosaveQueueMetrics` (ephemeral runtime counters), this struct
/// captures the state that must survive process restart so the system can
/// explain what remains unsaved after interruption.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutosaveBacklogState {
    /// Number of mutations pending persistence at the time of the snapshot.
    pub pending_mutations: usize,
    /// The last durable offset (event store seq or file entry count) that
    /// was successfully persisted.
    pub last_durable_offset: u64,
    /// Total number of entries in the session at snapshot time.
    pub total_entries: usize,
    /// The durability mode in effect when this state was captured.
    pub durability_mode: DurabilityMode,
    /// Cumulative count of successful flushes since session creation.
    pub flush_succeeded: u64,
    /// Cumulative count of failed flushes since session creation.
    pub flush_failed: u64,
    /// Cumulative count of coalesced (batched) mutations.
    pub coalesced_mutations: u64,
    /// Cumulative count of backpressure events (queue overflow).
    pub backpressure_events: u64,
    /// Trigger of the most recent flush attempt.
    pub last_flush_trigger: Option<String>,
    /// Duration of the most recent flush in milliseconds.
    pub last_flush_duration_ms: Option<u64>,
    /// Batch size of the most recent flush.
    pub last_flush_batch_size: usize,
    /// Whether a flush was in-flight when the state was captured.
    /// If true, the flush outcome is unknown and the mutations should
    /// be considered potentially lost.
    pub flush_in_flight: bool,
    /// ISO 8601 timestamp when this state was captured.
    pub captured_at: String,
    /// Number of consecutive flush failures (for retry/backoff logic).
    pub consecutive_failures: u32,
}

impl AutosaveBacklogState {
    /// Create a clean backlog state (nothing pending).
    pub fn clean(
        last_durable_offset: u64,
        total_entries: usize,
        durability_mode: DurabilityMode,
    ) -> Self {
        Self {
            pending_mutations: 0,
            last_durable_offset,
            total_entries,
            durability_mode,
            flush_succeeded: 0,
            flush_failed: 0,
            coalesced_mutations: 0,
            backpressure_events: 0,
            last_flush_trigger: None,
            last_flush_duration_ms: None,
            last_flush_batch_size: 0,
            flush_in_flight: false,
            captured_at: chrono_utc_now(),
            consecutive_failures: 0,
        }
    }

    /// Returns true if there are no pending mutations.
    pub const fn is_flushed(&self) -> bool {
        self.pending_mutations == 0 && !self.flush_in_flight
    }

    /// Returns true if there is work that may have been lost.
    pub const fn has_potential_data_loss(&self) -> bool {
        self.pending_mutations > 0 || self.flush_in_flight
    }
}

/// Returns an ISO 8601 UTC timestamp string.
fn chrono_utc_now() -> String {
    // Use a simple approach that doesn't require pulling in chrono as a
    // direct dependency in the DTO module — callers in session.rs can
    // provide timestamps from chrono directly.
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

// ============================================================================
// Session migration cutover types (VAL-SESS-006, VAL-SESS-011)
// ============================================================================

/// Outcome of a session migration attempt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationOutcome {
    /// Migration completed and authority has been flipped to the event store.
    Succeeded,
    /// Migration was explicitly rolled back; source authority remains active.
    RolledBack,
    /// Migration failed partway; partial target state must not be readable.
    Failed,
}

/// A durable correlation identifier that links a migration attempt to its
/// source store, target store, verification results, and final outcome.
///
/// This record is written to the **surviving authority** (the source store
/// for pre-cutover, or the event store for post-cutover) so that rollback
/// evidence is never lost to cleanup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationRecord {
    /// Unique correlation identifier for this migration attempt.
    pub correlation_id: String,
    /// Session ID being migrated.
    pub session_id: String,
    /// Path to the source (legacy) store.
    pub source_path: String,
    /// Kind of the source store.
    pub source_kind: PersistenceStoreKind,
    /// Path to the target (event store) directory.
    pub target_path: String,
    /// Number of entries found in the source.
    pub source_entry_count: u64,
    /// Number of entries successfully written to the target.
    pub target_entry_count: u64,
    /// Whether post-migration verification passed.
    pub verification_passed: bool,
    /// Verification errors encountered (empty if verification passed).
    pub verification_errors: Vec<String>,
    /// Final outcome of the migration.
    pub outcome: MigrationOutcome,
    /// ISO 8601 timestamp when the migration started.
    pub started_at: String,
    /// ISO 8601 timestamp when the migration completed (or failed/rolled back).
    pub completed_at: String,
    /// Human-readable reason for the outcome.
    pub reason: String,
}

/// A surviving rollback evidence record.
///
/// Written to the surviving authority (or a global rollback journal) when a
/// migration fails or is rolled back. This record proves which authority
/// remains active and why the migration did not succeed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RollbackRecord {
    /// Correlation ID linking this rollback to the original migration attempt.
    pub migration_correlation_id: String,
    /// Session ID that was being migrated.
    pub session_id: String,
    /// Path to the surviving (still-active) authority.
    pub surviving_authority_path: String,
    /// The authority that remains active after rollback.
    pub surviving_authority_kind: PersistenceStoreKind,
    /// Path to the target that was partially written (if any).
    pub target_path: Option<String>,
    /// How many entries had been written to the target before failure.
    pub target_entries_written: u64,
    /// ISO 8601 timestamp when the rollback occurred.
    pub rolled_back_at: String,
    /// Human-readable reason for the rollback.
    pub reason: String,
    /// Whether the partial target state was cleaned up.
    pub target_cleaned: bool,
}

// ============================================================================
// Verification ledger (VAL-WF-007, VAL-WF-014): durable verification
// evidence, acceptance mappings, attempt/workspace/input bindings, and
// restart-safe deferral/failure history.
// ============================================================================

/// The outcome of a verification attempt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationVerdict {
    /// Verification passed.
    Passed,
    /// Verification failed with a reason.
    Failed,
    /// Verification timed out.
    TimedOut,
    /// Verification was deferred to a later time or condition.
    Deferred,
    /// This verification was superseded by a newer attempt.
    Superseded,
}

/// A single immutable evidence record reference in the verification ledger.
///
/// Points to an artifact or evidence identifier produced by a verification
/// command execution. The actual evidence content is stored in the artifact
/// store; the ledger holds only the stable reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerificationEvidenceRef {
    /// Stable artifact/evidence identifier (e.g., "ev-171145-abc123").
    pub evidence_id: String,
    /// The verification command that produced this evidence.
    pub command: String,
    /// Exit code of the verification command.
    pub exit_code: i32,
    /// ISO 8601 timestamp when this evidence was produced.
    pub produced_at: String,
}

/// An explicit acceptance mapping linking evidence to acceptance criteria.
///
/// Each acceptance criterion (identified by `acceptance_id`) is explicitly
/// mapped to one or more evidence records that demonstrate its satisfaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AcceptanceMapping {
    /// Stable identifier for the acceptance criterion (e.g., "ac-1").
    pub acceptance_id: String,
    /// Human-readable description of what this criterion requires.
    pub description: Option<String>,
    /// Evidence record IDs that satisfy this acceptance criterion.
    pub satisfied_by_evidence: Vec<String>,
    /// Whether this acceptance criterion is considered satisfied.
    pub is_satisfied: bool,
}

/// A durable verification decision record.
///
/// This is the authoritative record that binds a verification verdict to
/// the task attempt, workspace state, and relevant inputs it evaluated.
/// It serves as the single source of truth for whether a task has been
/// verified and what evidence backs that decision (VAL-WF-007).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerificationDecision {
    /// Unique stable identifier for this verification decision.
    pub decision_id: String,
    /// The task ID this decision covers.
    pub task_id: String,
    /// The task attempt identity this decision covers.
    /// Multiple attempts may each have their own verification decisions.
    pub attempt_id: String,
    /// The run ID this decision is part of.
    pub run_id: String,
    /// The final verdict of this verification attempt.
    pub verdict: VerificationVerdict,
    /// Bound workspace snapshot or patch digest that this verdict was
    /// decided against. If the workspace changes after verification,
    /// this verdict is stale and completion should be blocked.
    pub workspace_patch_digest: String,
    /// The relevant input set reference (e.g., input snapshot hash or
    /// context reference ID).
    pub input_reference: Option<String>,
    /// Immutable evidence references produced by the verification commands.
    pub evidence_refs: Vec<VerificationEvidenceRef>,
    /// Explicit acceptance mappings linking acceptance criteria to evidence.
    pub acceptance_mappings: Vec<AcceptanceMapping>,
    /// When model-backed verification was used, the referenced context
    /// pack ID. None for command-only verification.
    pub context_pack_id: Option<String>,
    /// When model-backed verification was used, the inference receipt ID.
    /// None for command-only verification.
    pub inference_receipt_id: Option<String>,
    /// ISO 8601 timestamp when this decision was recorded.
    pub decided_at: String,
}

/// A single entry in the verification attempt history.
///
/// Every verification attempt (whether it passed, failed, timed out,
/// was deferred, superseded, or retried) appends a history entry.
/// The full history remains queryable after restart (VAL-WF-014).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerificationHistoryEntry {
    /// Monotonically increasing sequence number for ordering.
    pub seq: u64,
    /// Unique stable identifier for this history entry.
    pub entry_id: String,
    /// The task ID this history entry belongs to.
    pub task_id: String,
    /// The attempt identity this entry covers.
    pub attempt_id: String,
    /// The verification verdict for this attempt.
    pub verdict: VerificationVerdict,
    /// Human-readable reason for the verdict (e.g., failure message,
    /// deferral reason, timeout explanation).
    pub reason: String,
    /// The actor that produced this verification outcome (e.g., agent ID,
    /// "system", "human").
    pub actor: String,
    /// Evidence record IDs associated with this attempt.
    pub evidence_ids: Vec<String>,
    /// ISO 8601 timestamp when this entry was recorded.
    pub recorded_at: String,
    /// The next required action after this verdict. This makes it
    /// possible to explain why a task remained blocked and what
    /// needs to happen next.
    pub next_required_action: Option<String>,
    /// If this entry superseded a previous decision, the decision ID
    /// of the prior verification that was replaced.
    pub supersedes_decision_id: Option<String>,
}

/// Durable approval provenance for a verification override (VAL-WF-008).
///
/// Any verification bypass, override, or policy exception must be backed
/// by a durable approval record with actor identity, declared scope, and
/// correlation to the workflow entity being overridden.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerificationOverride {
    /// Unique stable identifier for this override.
    pub override_id: String,
    /// The task ID this override applies to.
    pub task_id: String,
    /// The verification decision ID being overridden.
    pub overridden_decision_id: String,
    /// The actor who approved this override.
    pub approved_by: String,
    /// The declared scope of the override (what it authorizes).
    pub scope: String,
    /// ISO 8601 timestamp when this override was approved.
    pub approved_at: String,
    /// The durable approval record ID that authorizes this override.
    pub approval_record_id: String,
    /// If this override itself was superseded by a newer one, the
    /// override ID of the replacement. A newer override must durably
    /// reference the prior override it replaces and cannot silently
    /// widen scope.
    pub supersedes_override_id: Option<String>,
    /// Human-readable justification for the override.
    pub justification: String,
}
