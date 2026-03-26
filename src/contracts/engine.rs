//! Engine contracts: authoritative service interfaces.
//!
//! These traits freeze the seams that surfaces and runtime planes are allowed to
//! depend on while keeping engine-owned behavior behind typed boundaries.

use crate::contracts::dto::{
    AcceptanceMapping, ContextPack, InterruptReason, InterruptResult, LegacyImportRequest,
    LegacyStoreRole, LegacyStoreValidation, MigrationRecord, ModelControl, PersistenceSnapshot,
    PersistenceStoreKind, QueueControl, QueueEnqueueResult, RollbackRecord, SessionEvent,
    SessionEventPayload, SessionIdentity, SessionProjection, VerificationDecision,
    VerificationEvidenceRef, VerificationHistoryEntry, VerificationOverride, VerificationVerdict,
    WorkerLaunchEnvelope, WorkerRuntimeKind,
};
use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Contract for conversation orchestration.
#[async_trait]
pub trait ConversationContract: Send + Sync {
    async fn session_identity(&self) -> Result<SessionIdentity>;
    async fn model_control(&self) -> Result<ModelControl>;
    async fn set_model_control(&self, control: ModelControl) -> Result<()>;
    async fn queue_control(&self) -> Result<QueueControl>;
    async fn set_queue_control(&self, control: QueueControl) -> Result<()>;
    async fn enqueue_steering(&self, message: String) -> Result<QueueEnqueueResult>;
    async fn enqueue_follow_up(&self, message: String) -> Result<QueueEnqueueResult>;
    async fn interrupt(&self, reason: InterruptReason) -> Result<InterruptResult>;
    async fn current_context(&self) -> Result<Option<ContextPack>>;
    async fn is_streaming(&self) -> bool;
    async fn is_compacting(&self) -> bool;
}

/// Contract for workflow orchestration.
#[async_trait]
pub trait WorkflowContract: Send + Sync {
    async fn create_run(&self, objective: String, tasks: Vec<TaskContract>) -> Result<String>;
    async fn run_status(&self, run_id: &str) -> Result<RunStatus>;
    async fn dispatch_run(
        &self,
        run_id: &str,
        options: DispatchOptions,
    ) -> Result<Vec<WorkerLaunchEnvelope>>;
    async fn cancel_run(&self, run_id: &str, reason: Option<String>) -> Result<()>;
    async fn submit_task(&self, request: SubmitTaskRequest) -> Result<SubmitTaskResult>;
    async fn append_evidence(&self, request: AppendEvidenceRequest) -> Result<()>;
    async fn state_digest(&self, task_id: Option<&str>) -> Result<StateDigestResult>;
    async fn acquire_lease(&self, task_id: &str, ttl_seconds: i64) -> Result<LeaseGrant>;
    async fn validate_lease(&self, task_id: &str, lease_id: &str, fence_token: u64)
    -> Result<bool>;

    // -- Verification ledger (VAL-WF-007, VAL-WF-014) --

    /// Record a verification decision and append a history entry.
    /// Returns the decision ID.
    async fn record_verification(&self, request: RecordVerificationRequest) -> Result<String>;

    /// Record a verification override (bypass) with approval provenance.
    /// Returns the override ID.
    async fn record_verification_override(&self, request: RecordOverrideRequest) -> Result<String>;

    /// Query the full verification history for a task, including
    /// the latest decision and any active overrides. This history
    /// survives restart and is the authoritative source for why
    /// a task is or is not verified.
    async fn verification_history(&self, task_id: &str) -> Result<VerificationHistoryResult>;
}

/// Workflow task contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskContract {
    pub task_id: String,
    pub objective: String,
    #[serde(default)]
    pub prerequisites: Vec<TaskPrerequisite>,
    pub verify_command: Option<String>,
    pub max_attempts: Option<u8>,
}

/// Task prerequisite specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskPrerequisite {
    pub task_id: String,
    #[serde(default)]
    pub trigger: PrerequisiteTrigger,
}

/// Trigger condition for a prerequisite.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PrerequisiteTrigger {
    #[default]
    OnSuccess,
    OnFailure,
    OnComplete,
}

/// Workflow run status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunStatus {
    pub run_id: String,
    pub lifecycle: RunLifecycle,
    pub wave_status: WaveStatus,
    pub task_ids: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Workflow lifecycle state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunLifecycle {
    Pending,
    Active,
    Completing,
    Complete,
    Canceled,
    Failed,
}

/// Workflow wave status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WaveStatus {
    Idle,
    Running,
    Verifying,
    Blocked,
    Complete,
}

/// Dispatch options for a workflow run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DispatchOptions {
    pub agent_id_prefix: Option<String>,
    pub lease_ttl_sec: Option<i64>,
    pub max_parallelism: Option<usize>,
}

/// Task submission payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubmitTaskRequest {
    pub task_id: String,
    pub lease_id: String,
    pub fence_token: u64,
    pub patch_digest: String,
    pub verify_run_id: String,
    pub verify_passed: Option<bool>,
    pub verify_timed_out: bool,
}

/// Result of submitting a completed task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubmitTaskResult {
    pub task_id: String,
    pub state: String,
    pub closed: bool,
}

/// Evidence append payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppendEvidenceRequest {
    pub task_id: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub artifact_ids: Vec<String>,
}

/// Digest of workflow state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StateDigestResult {
    pub schema: String,
    pub tasks: Vec<TaskStateDigest>,
    pub edge_count: usize,
    pub is_dag_valid: bool,
}

/// Digest of a single task state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStateDigest {
    pub task_id: String,
    pub state: String,
    pub attempts: u32,
    pub evidence_count: usize,
}

/// Lease grant result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LeaseGrant {
    pub lease_id: String,
    pub fence_token: u64,
    pub expires_at: i64,
}

// ============================================================================
// Verification ledger request/response types (VAL-WF-007, VAL-WF-014)
// ============================================================================

/// Request to record a verification decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecordVerificationRequest {
    /// The task ID being verified.
    pub task_id: String,
    /// The attempt identity this decision covers.
    pub attempt_id: String,
    /// The run ID this decision is part of.
    pub run_id: String,
    /// The final verdict.
    pub verdict: VerificationVerdict,
    /// The workspace patch digest this verdict was decided against.
    pub workspace_patch_digest: String,
    /// The relevant input set reference.
    pub input_reference: Option<String>,
    /// Evidence references produced by verification commands.
    pub evidence_refs: Vec<VerificationEvidenceRef>,
    /// Acceptance mappings linking acceptance criteria to evidence.
    pub acceptance_mappings: Vec<AcceptanceMapping>,
    /// Context pack ID for model-backed verification.
    pub context_pack_id: Option<String>,
    /// Inference receipt ID for model-backed verification.
    pub inference_receipt_id: Option<String>,
    /// Human-readable reason (required for non-Passed verdicts).
    pub reason: Option<String>,
    /// The actor that produced this verdict.
    pub actor: Option<String>,
    /// If this decision supersedes a previous one, its decision ID.
    pub supersedes_decision_id: Option<String>,
}

/// Request to record a verification override (bypass).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecordOverrideRequest {
    /// The task ID being overridden.
    pub task_id: String,
    /// The verification decision ID being overridden.
    pub overridden_decision_id: String,
    /// The actor who approved this override.
    pub approved_by: String,
    /// The declared scope of the override.
    pub scope: String,
    /// The durable approval record ID that authorizes this override.
    pub approval_record_id: String,
    /// If this supersedes a prior override, its override ID.
    pub supersedes_override_id: Option<String>,
    /// Human-readable justification.
    pub justification: String,
}

/// Query result for a task's verification history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VerificationHistoryResult {
    /// All verification history entries for the task, in sequence order.
    pub entries: Vec<VerificationHistoryEntry>,
    /// The latest verification decision, if any.
    pub latest_decision: Option<VerificationDecision>,
    /// Any active overrides, if any.
    pub active_overrides: Vec<VerificationOverride>,
}

/// Contract for worker runtime selection and launch.
#[async_trait]
pub trait WorkerRuntimeContract: Send + Sync {
    async fn select_runtime(&self, task: &TaskContract) -> Result<WorkerRuntimeKind>;
    async fn build_launch_envelope(
        &self,
        task: &TaskContract,
        run_id: &str,
        runtime: WorkerRuntimeKind,
    ) -> Result<WorkerLaunchEnvelope>;
    async fn prewarm_runtime(&self, kind: WorkerRuntimeKind) -> Result<()>;
    async fn is_runtime_available(&self, kind: WorkerRuntimeKind) -> bool;
    async fn shutdown_runtime(&self, kind: WorkerRuntimeKind) -> Result<()>;
}

/// Contract for session/workflow persistence.
///
/// This is the **single authoritative persistence boundary** for session
/// state. After cutover, all session writes flow through this contract and
/// all reads are served from rebuildable projections.
///
/// Legacy store access (JSONL, SQLite, V2 sidecar) is restricted to
/// import/export/migration/inspection roles via explicit role-gated methods.
#[async_trait]
pub trait PersistenceContract: Send + Sync {
    // -- Health and observability --

    async fn snapshot(&self) -> Result<PersistenceSnapshot>;
    async fn is_healthy(&self) -> bool;
    async fn rebuild_projections(&self) -> Result<()>;
    async fn last_persisted_offset(&self) -> u64;
    async fn pending_mutations(&self) -> usize;
    async fn flush(&self) -> Result<()>;

    // -- Authoritative session event store operations --

    /// Create a new session in the event store. Returns the session ID.
    async fn create_session(&self, session_id: String) -> Result<String>;

    /// Append an event to the authoritative session event store.
    /// Returns the assigned sequence number.
    async fn append_event(
        &self,
        session_id: &str,
        payload: SessionEventPayload,
        parent_event_id: Option<String>,
    ) -> Result<u64>;

    /// Get the current rebuildable session projection.
    async fn session_projection(&self, session_id: &str) -> Result<SessionProjection>;

    /// Read events from the session event store.
    /// Returns events in sequence order from the given offset.
    async fn read_events(
        &self,
        session_id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<SessionEvent>>;

    /// Read the active path (from root to current leaf) for a session.
    async fn read_active_path(&self, session_id: &str) -> Result<Vec<SessionEvent>>;

    /// Read the most recent N events (tail) for fast resume.
    async fn read_tail(&self, session_id: &str, count: u64) -> Result<Vec<SessionEvent>>;

    /// Compact a session by appending a compaction event.
    /// The compaction summary and continuity metadata become a typed event.
    async fn compact_session(
        &self,
        session_id: &str,
        summary: String,
        compacted_entry_count: u64,
        original_message_count: u64,
        continuity: Option<serde_json::Value>,
    ) -> Result<u64>;

    // -- Legacy store gating (import/export/migration/inspection only) --

    /// Validate a legacy store for import/migration/inspection use.
    /// Fails if the store would be used as a live authority.
    async fn validate_legacy_store(
        &self,
        path: &str,
        role: LegacyStoreRole,
    ) -> Result<LegacyStoreValidation>;

    /// Import a legacy session into the authoritative event store.
    /// The legacy store is read-only during import; writes go only to the event store.
    async fn import_legacy_session(&self, request: LegacyImportRequest) -> Result<String>;

    /// Export a session from the event store to a legacy format.
    /// The event store is read-only during export; writes go only to the legacy target.
    async fn export_session(
        &self,
        session_id: &str,
        target_kind: PersistenceStoreKind,
        target_path: &str,
    ) -> Result<()>;

    /// List sessions available in the event store (projection-backed).
    async fn list_sessions(&self) -> Result<Vec<SessionIdentity>>;

    // -- Atomic migration cutover (VAL-SESS-006, VAL-SESS-011) --

    /// Execute an atomic migration from a legacy session store to the
    /// authoritative event store.
    ///
    /// The migration is correlation-linked: a `MigrationRecord` is produced
    /// with a stable correlation ID, entry counts, verification results, and
    /// the final outcome. Authority flips to the event store **only** after
    /// verification succeeds. If migration fails or is interrupted, the
    /// source authority remains readable and a `RollbackRecord` is written
    /// to the surviving authority.
    ///
    /// Returns the `MigrationRecord` for audit/correlation.
    async fn migrate_session(&self, request: LegacyImportRequest) -> Result<MigrationRecord>;

    /// Retrieve the migration record for a session, if one exists.
    /// Returns `None` if the session was never migrated.
    async fn get_migration_record(&self, session_id: &str) -> Result<Option<MigrationRecord>>;

    /// Retrieve the rollback record for a failed/interrupted migration, if one exists.
    /// Returns `None` if no rollback occurred.
    async fn get_rollback_record(&self, session_id: &str) -> Result<Option<RollbackRecord>>;
}
