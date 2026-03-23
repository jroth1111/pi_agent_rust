//! Cross-cutting runtime plane contracts.

use crate::contracts::dto::{
    AdmissionControl, AdmissionGrant, ApprovalDecision, ApprovalPrompt, ContextFreshness,
    ContextPack, InferenceReceipt, WorkspaceSnapshot,
};
use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Contract for inference operations.
#[async_trait]
pub trait InferenceContract: Send + Sync {
    async fn available_models(&self) -> Result<Vec<ModelInfo>>;
    async fn get_model(&self, model_id: &str) -> Result<Option<ModelInfo>>;
    async fn create_session(&self, config: InferenceConfig) -> Result<InferenceSession>;
    async fn get_receipt(&self, session_id: &str) -> Result<InferenceReceipt>;
}

/// Model information exposed by the inference plane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: String,
    pub provider: String,
    pub display_name: String,
    pub supports_reasoning: bool,
    pub supports_images: bool,
    pub context_window: u32,
    pub max_output_tokens: Option<u32>,
}

/// Inference session configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceConfig {
    pub model_id: String,
    pub api_key: String,
    pub system_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub streaming: bool,
    pub tools: Vec<ToolDefinition>,
}

/// Tool definition surfaced to model providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Inference session handle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InferenceSession {
    pub session_id: String,
    pub model: ModelInfo,
    pub is_active: bool,
}

/// Contract for context/retrieval operations.
#[async_trait]
pub trait ContextContract: Send + Sync {
    async fn create_context_pack(&self, config: ContextPackConfig) -> Result<String>;
    async fn get_context_pack(&self, pack_id: &str) -> Result<Option<ContextPack>>;
    async fn update_context_pack(&self, pack_id: &str, updates: ContextPackUpdate) -> Result<()>;
    async fn get_freshness(&self, pack_id: &str) -> Result<ContextFreshness>;
    async fn validate_freshness(&self, pack_id: &str) -> Result<bool>;
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<RetrievalResult>>;
}

/// Context-pack creation request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextPackConfig {
    pub pack_id: String,
    pub source: String,
    pub token_budget: u64,
    pub sections: Vec<ContextSection>,
}

/// A section in a context pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextSection {
    pub name: String,
    pub content: String,
    pub tokens: u64,
    pub priority: u8,
}

/// Mutation applied to a context pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextPackUpdate {
    pub sections_to_update: Vec<ContextSection>,
    pub sections_to_remove: Vec<String>,
}

/// Retrieval result with a floating-point score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalResult {
    pub source: String,
    pub content: String,
    pub score: f64,
}

/// Contract for workspace management.
#[async_trait]
pub trait WorkspaceContract: Send + Sync {
    async fn create_workspace(&self, config: WorkspaceConfig) -> Result<WorkspaceSnapshot>;
    async fn get_workspace(&self, workspace_id: &str) -> Result<Option<WorkspaceSnapshot>>;
    async fn current_workspace(&self) -> Result<Option<WorkspaceSnapshot>>;
    async fn create_worktree(
        &self,
        workspace_id: &str,
        config: WorktreeConfig,
    ) -> Result<WorktreeInfo>;
    async fn apply_patch(&self, workspace_id: &str, patch: Patch) -> Result<PatchResult>;
    async fn get_status(&self, workspace_id: &str) -> Result<WorkspaceStatus>;
}

/// Workspace creation request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    pub name: String,
    pub repo_path: String,
    pub branch: Option<String>,
}

/// Worktree creation request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeConfig {
    pub name: String,
    pub branch: String,
    pub create_branch: bool,
}

/// Worktree metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub name: String,
    pub path: String,
    pub branch: String,
    pub head: String,
}

/// Patch payload for workspace application.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    pub content: String,
    pub source_ref: Option<String>,
    pub target_ref: Option<String>,
}

/// Result of applying a patch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PatchResult {
    pub success: bool,
    pub conflicts: Vec<ConflictInfo>,
    pub changed_files: Vec<String>,
}

/// Conflict descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConflictInfo {
    pub path: String,
    pub conflict_type: ConflictType,
    pub description: String,
}

/// Merge conflict type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConflictType {
    Text,
    Binary,
    DeleteModify,
}

/// Workspace status summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub is_clean: bool,
    pub uncommitted_changes: usize,
    pub current_branch: String,
    pub issues: Vec<String>,
}

/// Contract for host execution operations.
#[async_trait]
pub trait HostExecutionContract: Send + Sync {
    async fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult>;
    async fn read_file(&self, path: &str, options: FileReadOptions) -> Result<FileContent>;
    async fn write_file(&self, path: &str, content: &FileContent) -> Result<()>;
    async fn file_exists(&self, path: &str) -> Result<bool>;
    async fn get_receipt(&self, execution_id: &str) -> Result<Option<ExecutionReceipt>>;
}

/// Command execution request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRequest {
    pub command: String,
    pub arguments: Vec<String>,
    pub working_dir: Option<String>,
    pub env: HashMap<String, String>,
    pub timeout_seconds: Option<u32>,
    pub capture_output: bool,
}

/// Result of a command execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionResult {
    pub execution_id: String,
    pub exit_code: i32,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub duration_ms: u64,
    pub timed_out: bool,
}

/// File-read options for host execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileReadOptions {
    pub max_bytes: Option<u64>,
    pub max_lines: Option<u32>,
    pub include_line_numbers: bool,
    pub offset: Option<u64>,
}

/// File content returned by host execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    pub text: String,
    pub line_count: u64,
    pub byte_count: u64,
    pub was_truncated: bool,
}

/// Durable execution receipt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionReceipt {
    pub execution_id: String,
    pub command: String,
    pub working_dir: Option<String>,
    pub exit_code: i32,
    pub total_duration_ms: u64,
    pub started_at: i64,
    pub ended_at: i64,
}

/// Contract for identity and secret materialization.
#[async_trait]
pub trait IdentityContract: Send + Sync {
    async fn current_identity(&self) -> Result<ActorIdentity>;
    async fn get_identity(&self, identity_id: &str) -> Result<Option<ActorIdentity>>;
    async fn resolve_credentials(&self, provider: &str) -> Result<Credentials>;
    async fn create_secret_handle(&self, request: SecretHandleRequest) -> Result<SecretHandle>;
    async fn get_secret_handle(&self, handle_id: &str) -> Result<Option<SecretHandle>>;
    async fn is_handle_valid(&self, handle_id: &str) -> Result<bool>;
}

/// Actor identity metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActorIdentity {
    pub identity_id: String,
    pub display_name: String,
    pub actor_type: ActorType,
    pub created_at: i64,
    pub metadata: HashMap<String, String>,
}

/// Actor type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActorType {
    Human,
    Agent,
    Service,
    Extension,
}

/// Provider credentials resolved by the identity plane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub provider: String,
    pub api_key: Option<String>,
    pub bearer_token: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
}

/// Secret-handle creation request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecretHandleRequest {
    pub name: String,
    pub secret_type: SecretType,
    pub ttl_seconds: u64,
}

/// Secret handle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecretHandle {
    pub handle_id: String,
    pub name: String,
    pub secret_type: SecretType,
    pub expires_at: i64,
}

/// Secret type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretType {
    ApiKey,
    BearerToken,
    OAuthToken,
    Generic,
}

/// Contract for capability and approval decisions.
#[async_trait]
pub trait CapabilityContract: Send + Sync {
    async fn evaluate(&self, request: CapabilityRequest) -> Result<CapabilityDecision>;
    async fn requires_approval(&self, action: &str) -> bool;
    async fn create_approval_prompt(&self, action: &str, context: &str) -> Result<ApprovalPrompt>;
    async fn submit_approval_decision(
        &self,
        prompt_id: &str,
        decision: ApprovalDecision,
    ) -> Result<()>;
    async fn approval_history(&self, limit: usize) -> Result<Vec<ApprovalRecord>>;
}

/// Capability request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRequest {
    pub capability: String,
    pub extension_id: Option<String>,
    pub context: String,
}

/// Capability decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDecision {
    pub allowed: bool,
    pub reason: String,
    pub requires_approval: bool,
    pub approval_prompt_id: Option<String>,
}

/// Approval history record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRecord {
    pub record_id: String,
    pub prompt_id: String,
    pub decision: ApprovalDecision,
    pub timestamp: i64,
    pub actor: String,
}

/// Contract for admission control.
#[async_trait]
pub trait AdmissionContract: Send + Sync {
    async fn request_admission(&self, request: AdmissionControl) -> Result<AdmissionGrant>;
    async fn get_state(&self) -> Result<AdmissionState>;
    async fn release_admission(&self, grant_id: &str) -> Result<()>;
    async fn get_history(&self, limit: usize) -> Result<Vec<AdmissionRecord>>;
}

/// Admission-controller state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionState {
    pub current_capacity: u64,
    pub allocated: u64,
    pub pending_requests: usize,
    pub active_grants: usize,
}

/// Historical admission decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionRecord {
    pub request_id: String,
    pub grant_id: Option<String>,
    pub granted: bool,
    pub decided_at: i64,
    pub reason: String,
}

/// Contract for extension-runtime management.
#[async_trait]
pub trait ExtensionRuntimeContract: Send + Sync {
    async fn initialize(&self, config: ExtensionRuntimeConfig) -> Result<()>;
    async fn load_extension(&self, spec: ExtensionLoadSpec) -> Result<String>;
    async fn unload_extension(&self, extension_id: &str) -> Result<()>;
    async fn get_runtime_state(&self) -> Result<RuntimeState>;
}

/// Extension-runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRuntimeConfig {
    pub runtime_kind: RuntimeKind,
    pub memory_limit: Option<u64>,
    pub debug: bool,
    pub policy: ExtensionPolicy,
}

/// Runtime kind used for extension execution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    QuickJs,
    NativeRust,
    Wasm,
}

/// Extension load specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionLoadSpec {
    pub path: String,
    pub name: String,
    pub version: Option<String>,
}

/// Extension policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPolicy {
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
    pub prompt_for_unknown: bool,
}

/// Extension-runtime state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeState {
    pub is_initialized: bool,
    pub extension_count: usize,
    pub memory_used: u64,
    pub memory_limit: Option<u64>,
}
