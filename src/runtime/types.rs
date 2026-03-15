use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

pub type RunId = String;
pub type TaskId = String;
pub type JobId = String;
pub type PlanId = String;
pub type ApprovalId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Created,
    Planning,
    Dispatching,
    Running,
    Verifying,
    AwaitingHuman,
    Recovering,
    Completed,
    Failed,
    Canceled,
}

impl RunPhase {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Draft,
    Blocked,
    Ready,
    Leased,
    Executing,
    Verifying,
    AwaitingHuman,
    Recoverable,
    Succeeded,
    Failed,
    Canceled,
    Superseded,
}

impl TaskState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Canceled | Self::Superseded
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    TurnEngine,
    Verification,
    BackgroundCommand,
    BackgroundSummary,
    BackgroundIndex,
    WorkerLease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    Supervised,
    Guarded,
    Autonomous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationReason {
    PlanExecution,
    TaskReady,
    VerificationRetry,
    BackgroundTaskCompletion,
    ApprovalResolved,
    StuckReflection,
    WakeTimer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunBudgets {
    pub max_parallelism: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost_microusd: Option<u64>,
}

impl Default for RunBudgets {
    fn default() -> Self {
        Self {
            max_parallelism: 4,
            max_steps: None,
            max_cost_microusd: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunConstraints {
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(default)]
    pub forbid_paths: Vec<String>,
    #[serde(default)]
    pub allow_network_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunSpec {
    pub run_id: RunId,
    pub objective: String,
    pub root_workspace: PathBuf,
    pub policy_profile: String,
    pub model_profile: String,
    pub budgets: RunBudgets,
    pub constraints: RunConstraints,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunSummary {
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub recent_actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(default)]
    pub task_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanArtifact {
    pub plan_id: PlanId,
    pub digest: String,
    pub objective: String,
    #[serde(default)]
    pub task_drafts: Vec<String>,
    #[serde(default)]
    pub touched_paths: Vec<PathBuf>,
    #[serde(default)]
    pub test_strategy: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub produced_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifySpec {
    pub command: String,
    pub timeout_sec: u32,
    #[serde(default)]
    pub acceptance_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskConstraints {
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_touched_files: Option<u16>,
    #[serde(default)]
    pub forbid_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskSpec {
    pub task_id: TaskId,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub planned_touches: Vec<PathBuf>,
    pub verify: VerifySpec,
    pub autonomy: AutonomyLevel,
    pub constraints: TaskConstraints,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseRecord {
    pub lease_id: String,
    pub owner: String,
    pub fence_token: u64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailureRecord {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRuntime {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_reason: Option<ContinuationReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<FailureRecord>,
}

impl Default for TaskRuntime {
    fn default() -> Self {
        Self {
            state: TaskState::Draft,
            lease: None,
            retry_at: None,
            continuation_reason: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskNode {
    pub spec: TaskSpec,
    pub runtime: TaskRuntime,
    #[serde(default)]
    pub deps: Vec<TaskId>,
    #[serde(default)]
    pub children: Vec<TaskId>,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
}

impl TaskNode {
    pub fn new(spec: TaskSpec) -> Self {
        Self {
            spec,
            runtime: TaskRuntime::default(),
            deps: Vec::new(),
            children: Vec::new(),
            evidence_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobRecord {
    pub job_id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<LeaseRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub result_artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalCheckpoint {
    pub approval_id: ApprovalId,
    pub reason: String,
    pub context: String,
    pub state: ApprovalState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelSelector {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelProfile {
    pub planner: ModelSelector,
    pub executor: ModelSelector,
    pub verifier: ModelSelector,
    pub summarizer: ModelSelector,
    pub background: ModelSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunSnapshot {
    pub spec: RunSpec,
    pub phase: RunPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanArtifact>,
    #[serde(default)]
    pub tasks: BTreeMap<TaskId, TaskNode>,
    #[serde(default)]
    pub jobs: BTreeMap<JobId, JobRecord>,
    #[serde(default)]
    pub approvals: BTreeMap<ApprovalId, ApprovalCheckpoint>,
    #[serde(default)]
    pub ready_queue: VecDeque<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<DateTime<Utc>>,
    pub summary: RunSummary,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RunSnapshot {
    pub fn new(spec: RunSpec) -> Self {
        let now = Utc::now();
        Self {
            spec,
            phase: RunPhase::Created,
            plan: None,
            tasks: BTreeMap::new(),
            jobs: BTreeMap::new(),
            approvals: BTreeMap::new(),
            ready_queue: VecDeque::new(),
            wake_at: None,
            summary: RunSummary::default(),
            version: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> RunSpec {
        RunSpec {
            run_id: "run-1".to_string(),
            objective: "Ship runtime migration".to_string(),
            root_workspace: PathBuf::from("/tmp/pi"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn run_snapshot_new_starts_created() {
        let snapshot = RunSnapshot::new(sample_spec());
        assert_eq!(snapshot.phase, RunPhase::Created);
        assert!(snapshot.tasks.is_empty());
        assert!(snapshot.ready_queue.is_empty());
    }

    #[test]
    fn task_runtime_defaults_to_draft() {
        let runtime = TaskRuntime::default();
        assert_eq!(runtime.state, TaskState::Draft);
        assert!(runtime.lease.is_none());
    }
}
