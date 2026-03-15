use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

pub type RunId = String;
pub type TaskId = String;
pub type JobId = String;
pub type PlanId = String;
pub type ApprovalId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTier {
    Inline,
    Wave,
    Hierarchical,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunLifecycle {
    Pending,
    Running,
    Blocked,
    AwaitingHuman,
    Succeeded,
    Failed,
    Canceled,
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct RunConstraints {
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(default)]
    pub forbid_paths: Vec<String>,
    #[serde(default)]
    pub allow_network_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunSpec {
    pub run_id: RunId,
    pub objective: String,
    pub root_workspace: PathBuf,
    pub policy_profile: String,
    pub model_profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_verify_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_verify_timeout_sec: Option<u32>,
    pub budgets: RunBudgets,
    pub constraints: RunConstraints,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct TaskReport {
    pub task_id: String,
    pub attempt: u8,
    pub summary: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    pub patch_digest: String,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
    #[serde(default)]
    pub acceptance_ids: Vec<String>,
    pub verify_command: String,
    pub verify_exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    pub workspace_snapshot: String,
    pub generated_at: DateTime<Utc>,
}

impl TaskReport {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            attempt: 0,
            summary: String::new(),
            changed_files: Vec::new(),
            patch_digest: String::new(),
            evidence_ids: Vec::new(),
            acceptance_ids: Vec::new(),
            verify_command: String::new(),
            verify_exit_code: 0,
            failure_class: None,
            blockers: Vec::new(),
            workspace_snapshot: String::new(),
            generated_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WaveStatus {
    pub wave_id: String,
    #[serde(default)]
    pub task_ids: Vec<String>,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunVerifyScopeKind {
    Run,
    Wave,
    Subrun,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunVerifyStatus {
    pub scope_id: String,
    pub scope_kind: RunVerifyScopeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subrun_id: Option<String>,
    pub command: String,
    pub timeout_sec: u32,
    pub exit_code: i32,
    pub ok: bool,
    pub summary: String,
    pub duration_ms: u64,
    pub generated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubrunPlan {
    pub subrun_id: String,
    #[serde(default)]
    pub task_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunDispatchState {
    pub selected_tier: ExecutionTier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_subrun_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_wave: Option<WaveStatus>,
    #[serde(default)]
    pub planned_subruns: Vec<SubrunPlan>,
    #[serde(default)]
    pub task_reports: BTreeMap<String, TaskReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_run_verify: Option<RunVerifyStatus>,
}

impl Default for RunDispatchState {
    fn default() -> Self {
        Self {
            selected_tier: ExecutionTier::Inline,
            active_subrun_id: None,
            active_wave: None,
            planned_subruns: Vec::new(),
            task_reports: BTreeMap::new(),
            latest_run_verify: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunStatus {
    pub run_id: String,
    pub objective: String,
    pub selected_tier: ExecutionTier,
    pub lifecycle: RunLifecycle,
    #[serde(default)]
    pub run_verify_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_verify_timeout_sec: Option<u32>,
    #[serde(default = "RunStatus::default_max_parallelism")]
    pub max_parallelism: usize,
    #[serde(default)]
    pub task_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_subrun_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_wave: Option<WaveStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planned_subruns: Vec<SubrunPlan>,
    #[serde(default)]
    pub task_counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub task_reports: BTreeMap<String, TaskReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_run_verify: Option<RunVerifyStatus>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RunStatus {
    pub const DEFAULT_MAX_PARALLELISM: usize = 4;

    const fn default_max_parallelism() -> usize {
        Self::DEFAULT_MAX_PARALLELISM
    }

    pub fn new(
        run_id: impl Into<String>,
        objective: impl Into<String>,
        selected_tier: ExecutionTier,
    ) -> Self {
        let now = Utc::now();
        Self {
            run_id: run_id.into(),
            objective: objective.into(),
            selected_tier,
            lifecycle: RunLifecycle::Pending,
            run_verify_command: String::new(),
            run_verify_timeout_sec: None,
            max_parallelism: Self::DEFAULT_MAX_PARALLELISM,
            task_ids: Vec::new(),
            active_subrun_id: None,
            active_wave: None,
            planned_subruns: Vec::new(),
            task_counts: BTreeMap::new(),
            task_reports: BTreeMap::new(),
            latest_run_verify: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    pub fn set_task_count(&mut self, state_label: impl Into<String>, count: usize) {
        self.task_counts.insert(state_label.into(), count);
        self.touch();
    }

    pub fn upsert_task_report(&mut self, report: TaskReport) {
        self.task_reports.insert(report.task_id.clone(), report);
        self.touch();
    }

    pub fn effective_max_parallelism(&self) -> usize {
        match self.selected_tier {
            ExecutionTier::Inline => 1,
            ExecutionTier::Wave | ExecutionTier::Hierarchical => self.max_parallelism.max(1),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct VerifySpec {
    pub command: String,
    pub timeout_sec: u32,
    #[serde(default)]
    pub acceptance_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskConstraints {
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_touched_files: Option<u16>,
    #[serde(default)]
    pub forbid_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct LeaseRecord {
    pub lease_id: String,
    pub owner: String,
    pub fence_token: u64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FailureRecord {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct ApprovalCheckpoint {
    pub approval_id: ApprovalId,
    pub reason: String,
    pub context: String,
    pub state: ApprovalState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelector {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfile {
    pub planner: ModelSelector,
    pub executor: ModelSelector,
    pub verifier: ModelSelector,
    pub summarizer: ModelSelector,
    pub background: ModelSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub spec: RunSpec,
    pub phase: RunPhase,
    #[serde(default)]
    pub plan_required: bool,
    #[serde(default)]
    pub plan_accepted: bool,
    #[serde(default = "RunSnapshot::default_auto_proceed_after_planning")]
    pub auto_proceed_after_planning: bool,
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
    #[serde(default)]
    pub dispatch: RunDispatchState,
    pub summary: RunSummary,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RunSnapshot {
    const fn default_auto_proceed_after_planning() -> bool {
        true
    }

    pub fn new(spec: RunSpec) -> Self {
        let now = Utc::now();
        Self {
            spec,
            phase: RunPhase::Created,
            plan_required: false,
            plan_accepted: false,
            auto_proceed_after_planning: Self::default_auto_proceed_after_planning(),
            plan: None,
            tasks: BTreeMap::new(),
            jobs: BTreeMap::new(),
            approvals: BTreeMap::new(),
            ready_queue: VecDeque::new(),
            wake_at: None,
            dispatch: RunDispatchState::default(),
            summary: RunSummary::default(),
            version: 0,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn task_ids(&self) -> Vec<String> {
        self.tasks.keys().cloned().collect()
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
            run_verify_command: Some("cargo test".to_string()),
            run_verify_timeout_sec: Some(60),
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

    #[test]
    fn run_status_upserts_reports_and_counts() {
        let mut run = RunStatus::new("run-1", "Ship orchestration", ExecutionTier::Wave);
        run.set_task_count("ready", 2);
        run.upsert_task_report(TaskReport {
            task_id: "task-a".to_string(),
            attempt: 1,
            summary: "done".to_string(),
            changed_files: vec!["src/rpc.rs".to_string()],
            patch_digest: "digest".to_string(),
            evidence_ids: vec!["ev-1".to_string()],
            acceptance_ids: vec!["ac-1".to_string()],
            verify_command: "cargo test".to_string(),
            verify_exit_code: 0,
            failure_class: None,
            blockers: Vec::new(),
            workspace_snapshot: "abc123".to_string(),
            generated_at: Utc::now(),
        });

        assert_eq!(run.task_counts.get("ready"), Some(&2));
        assert_eq!(
            run.task_reports
                .get("task-a")
                .map(|report| report.summary.as_str()),
            Some("done")
        );
    }
}
