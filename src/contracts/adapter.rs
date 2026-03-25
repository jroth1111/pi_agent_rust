//! Contract adapters that avoid direct RPC-state authority.
//!
//! The long-term goal is to route surfaces through dedicated services. These
//! adapters are intentionally thin and only delegate the workflow methods that
//! already have a migrated service path.

use crate::contracts::dto::{
    ContextPack, InterruptReason, InterruptResult, ModelControl, QueueControl, QueueEnqueueResult,
    QueueKind, QueueMode, SessionIdentity, WorkerLaunchEnvelope,
};
use crate::contracts::engine::{
    AppendEvidenceRequest, ConversationContract, DispatchOptions, LeaseGrant, RunStatus,
    StateDigestResult, SubmitTaskRequest, SubmitTaskResult, TaskStateDigest, WorkflowContract,
};
use crate::error::{Error, Result};
use crate::services::reliability_service::ReliabilityService;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Minimal conversation adapter. Queue mutation remains unwired until the
/// surface/kernel composition layer lands.
pub struct RpcConversationAdapter {
    session_handle: Arc<Mutex<crate::session::Session>>,
    next_seq: AtomicU64,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
}

impl RpcConversationAdapter {
    #[must_use]
    pub(crate) fn new(
        _shared_state: Arc<Mutex<crate::rpc::RpcSharedState>>,
        session_handle: Arc<Mutex<crate::session::Session>>,
        is_streaming: Arc<AtomicBool>,
        is_compacting: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session_handle,
            next_seq: AtomicU64::new(1),
            is_streaming,
            is_compacting,
        }
    }

    fn next_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::SeqCst)
    }
}

#[async_trait]
impl ConversationContract for RpcConversationAdapter {
    async fn session_identity(&self) -> Result<SessionIdentity> {
        let session = self
            .session_handle
            .lock()
            .map_err(|e| Error::session(format!("session lock failed: {e}")))?;
        Ok(SessionIdentity {
            session_id: session.header.id.clone(),
            name: None,
            path: session.path.as_ref().map(|path| path.display().to_string()),
        })
    }

    async fn model_control(&self) -> Result<ModelControl> {
        let session = self
            .session_handle
            .lock()
            .map_err(|e| Error::session(format!("session lock failed: {e}")))?;
        Ok(ModelControl {
            model_id: session.header.model_id.clone().unwrap_or_default(),
            provider: session.header.provider.clone().unwrap_or_default(),
            thinking_level: Default::default(),
            thinking_budget_tokens: None,
        })
    }

    async fn set_model_control(&self, _control: ModelControl) -> Result<()> {
        Err(Error::validation(
            "contract adapter model mutation is not wired; use kernel services",
        ))
    }

    async fn queue_control(&self) -> Result<QueueControl> {
        Ok(QueueControl {
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            pending_steering: 0,
            pending_follow_up: 0,
        })
    }

    async fn set_queue_control(&self, _control: QueueControl) -> Result<()> {
        Err(Error::validation(
            "contract adapter queue control is not wired; use kernel services",
        ))
    }

    async fn enqueue_steering(&self, _message: String) -> Result<QueueEnqueueResult> {
        Ok(QueueEnqueueResult {
            seq: self.next_seq(),
            queue: QueueKind::Steering,
        })
    }

    async fn enqueue_follow_up(&self, _message: String) -> Result<QueueEnqueueResult> {
        Ok(QueueEnqueueResult {
            seq: self.next_seq(),
            queue: QueueKind::FollowUp,
        })
    }

    async fn interrupt(&self, reason: InterruptReason) -> Result<InterruptResult> {
        Ok(InterruptResult {
            success: true,
            reason,
            cancelled_count: 0,
        })
    }

    async fn current_context(&self) -> Result<Option<ContextPack>> {
        Ok(None)
    }

    async fn is_streaming(&self) -> bool {
        self.is_streaming.load(Ordering::SeqCst)
    }

    async fn is_compacting(&self) -> bool {
        self.is_compacting.load(Ordering::SeqCst)
    }
}

/// Workflow adapter that delegates the task-lifecycle methods to
/// [`ReliabilityService`].
pub struct RpcWorkflowAdapter {
    reliability_service: Arc<ReliabilityService>,
    _orchestration_state: Arc<Mutex<crate::rpc::RpcOrchestrationState>>,
}

impl RpcWorkflowAdapter {
    #[must_use]
    pub(crate) fn new(
        reliability_state: Arc<Mutex<crate::rpc::RpcReliabilityState>>,
        orchestration_state: Arc<Mutex<crate::rpc::RpcOrchestrationState>>,
    ) -> Self {
        Self {
            reliability_service: Arc::new(ReliabilityService::new(reliability_state)),
            _orchestration_state: orchestration_state,
        }
    }
}

#[async_trait]
impl WorkflowContract for RpcWorkflowAdapter {
    async fn create_run(
        &self,
        _objective: String,
        _tasks: Vec<crate::contracts::engine::TaskContract>,
    ) -> Result<String> {
        Err(Error::validation(
            "contract adapter run creation is not wired; use kernel services",
        ))
    }

    async fn run_status(&self, _run_id: &str) -> Result<RunStatus> {
        Err(Error::validation(
            "contract adapter run status is not wired; use kernel services",
        ))
    }

    async fn dispatch_run(
        &self,
        _run_id: &str,
        _options: DispatchOptions,
    ) -> Result<Vec<WorkerLaunchEnvelope>> {
        Err(Error::validation(
            "contract adapter run dispatch is not wired; use kernel services",
        ))
    }

    async fn cancel_run(&self, _run_id: &str, _reason: Option<String>) -> Result<()> {
        Err(Error::validation(
            "contract adapter run cancellation is not wired; use kernel services",
        ))
    }

    async fn submit_task(&self, request: SubmitTaskRequest) -> Result<SubmitTaskResult> {
        let rpc_request = crate::rpc::SubmitTaskRequest {
            task_id: request.task_id,
            lease_id: request.lease_id,
            fence_token: request.fence_token,
            patch_digest: request.patch_digest,
            verify_run_id: request.verify_run_id,
            verify_passed: request.verify_passed,
            verify_timed_out: request.verify_timed_out,
            failure_class: None,
            changed_files: Vec::new(),
            symbol_drift_violations: Vec::new(),
            close: None,
        };
        let result = self.reliability_service.submit_task(rpc_request).await?;
        Ok(SubmitTaskResult {
            task_id: result.task_id,
            state: result.state,
            closed: result.close.approved,
        })
    }

    async fn append_evidence(&self, request: AppendEvidenceRequest) -> Result<()> {
        let rpc_request = crate::rpc::AppendEvidenceRequest {
            task_id: request.task_id,
            command: request.command,
            exit_code: request.exit_code,
            stdout: request.stdout,
            stderr: request.stderr,
            artifact_ids: request.artifact_ids,
            env_id: None,
        };
        let _ = self
            .reliability_service
            .append_evidence(rpc_request)
            .await?;
        Ok(())
    }

    async fn state_digest(&self, task_id: Option<&str>) -> Result<StateDigestResult> {
        let task_id = match task_id {
            Some(id) => id.to_string(),
            None => self
                .reliability_service
                .first_task_id()
                .await?
                .ok_or_else(|| Error::validation("no tasks available for digest"))?,
        };
        let digest = self.reliability_service.get_state_digest(&task_id).await?;
        Ok(StateDigestResult {
            schema: "rpc-reliability/v1".to_string(),
            tasks: vec![TaskStateDigest {
                task_id,
                state: digest.phase,
                attempts: 0,
                evidence_count: 0,
            }],
            edge_count: 0,
            is_dag_valid: true,
        })
    }

    async fn acquire_lease(&self, task_id: &str, ttl_seconds: i64) -> Result<LeaseGrant> {
        let grant = self
            .reliability_service
            .request_dispatch_existing(task_id, &format!("contract-adapter:{task_id}"), ttl_seconds)
            .await?;
        Ok(LeaseGrant {
            lease_id: grant.lease_id,
            fence_token: grant.fence_token,
            expires_at: grant.expires_at.timestamp(),
        })
    }

    async fn validate_lease(
        &self,
        _task_id: &str,
        lease_id: &str,
        fence_token: u64,
    ) -> Result<bool> {
        self.reliability_service
            .validate_fence(lease_id, fence_token)
            .await
    }
}

/// TUI conversation adapter that provides ConversationContract for the interactive terminal.
/// This adapter wires TUI operations through the shared contract instead of direct state mutation.
pub struct TuiConversationAdapter {
    session_handle: Arc<Mutex<crate::session::Session>>,
    config_handle: Arc<Mutex<crate::config::Config>>,
    next_seq: AtomicU64,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
}

impl TuiConversationAdapter {
    #[must_use]
    pub(crate) const fn new(
        session_handle: Arc<Mutex<crate::session::Session>>,
        config_handle: Arc<Mutex<crate::config::Config>>,
        is_streaming: Arc<AtomicBool>,
        is_compacting: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session_handle,
            config_handle,
            next_seq: AtomicU64::new(1),
            is_streaming,
            is_compacting,
        }
    }

    fn next_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::SeqCst)
    }
}

#[async_trait]
impl ConversationContract for TuiConversationAdapter {
    async fn session_identity(&self) -> Result<SessionIdentity> {
        let session = self
            .session_handle
            .lock()
            .map_err(|e| Error::session(format!("session lock failed: {e}")))?;
        Ok(SessionIdentity {
            session_id: session.header.id.clone(),
            name: None,
            path: session.path.as_ref().map(|path| path.display().to_string()),
        })
    }

    async fn model_control(&self) -> Result<ModelControl> {
        let session = self
            .session_handle
            .lock()
            .map_err(|e| Error::session(format!("session lock failed: {e}")))?;
        Ok(ModelControl {
            model_id: session.header.model_id.clone().unwrap_or_default(),
            provider: session.header.provider.clone().unwrap_or_default(),
            thinking_level: session
                .header
                .thinking_level
                .as_ref()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default(),
            thinking_budget_tokens: None,
        })
    }

    async fn set_model_control(&self, control: ModelControl) -> Result<()> {
        let mut session = self
            .session_handle
            .lock()
            .map_err(|e| Error::session(format!("session lock failed: {e}")))?;
        session.header.model_id = Some(control.model_id);
        session.header.provider = Some(control.provider);
        session.header.thinking_level = Some(control.thinking_level.to_string());
        Ok(())
    }

    async fn queue_control(&self) -> Result<QueueControl> {
        // TUI uses default one-at-a-time mode
        Ok(QueueControl {
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            pending_steering: 0,
            pending_follow_up: 0,
        })
    }

    async fn set_queue_control(&self, _control: QueueControl) -> Result<()> {
        // Queue control settings are not persisted in TUI mode
        Ok(())
    }

    async fn enqueue_steering(&self, _message: String) -> Result<QueueEnqueueResult> {
        // TUI handles steering via input queue
        Ok(QueueEnqueueResult {
            seq: self.next_seq(),
            queue: QueueKind::Steering,
        })
    }

    async fn enqueue_follow_up(&self, _message: String) -> Result<QueueEnqueueResult> {
        // TUI handles follow-up via input queue
        Ok(QueueEnqueueResult {
            seq: self.next_seq(),
            queue: QueueKind::FollowUp,
        })
    }

    async fn interrupt(&self, reason: InterruptReason) -> Result<InterruptResult> {
        // TUI handles interrupt via abort handle
        Ok(InterruptResult {
            success: true,
            reason,
            cancelled_count: 0,
        })
    }

    async fn current_context(&self) -> Result<Option<ContextPack>> {
        Ok(None)
    }

    async fn is_streaming(&self) -> bool {
        self.is_streaming.load(Ordering::SeqCst)
    }

    async fn is_compacting(&self) -> bool {
        self.is_compacting.load(Ordering::SeqCst)
    }
}
