//! Engine contracts: authoritative service interfaces.
//!
//! These traits freeze the seams that surfaces and runtime planes are allowed to
//! depend on while keeping engine-owned behavior behind typed boundaries.

use crate::contracts::dto::{
    ContextPack, InterruptReason, InterruptResult, LegacyImportRequest, LegacyStoreRole,
    LegacyStoreValidation, ModelControl, PersistenceSnapshot, PersistenceStoreKind, QueueControl,
    QueueEnqueueResult, SessionEvent, SessionEventPayload, SessionIdentity, SessionProjection,
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
}
