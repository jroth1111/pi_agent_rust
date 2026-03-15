use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const RUN_SPEC_SCHEMA: &str = "pi.runtime.run_spec.v1";
pub const RUN_SNAPSHOT_SCHEMA: &str = "pi.runtime.run_snapshot.v1";
pub const PLAN_ARTIFACT_SCHEMA: &str = "pi.runtime.plan_artifact.v1";
pub const TASK_NODE_SCHEMA: &str = "pi.runtime.task_node.v1";
pub const TASK_RUNTIME_SCHEMA: &str = "pi.runtime.task_runtime.v1";
pub const JOB_RECORD_SCHEMA: &str = "pi.runtime.job_record.v1";
pub const POLICY_VERDICT_SCHEMA: &str = "pi.runtime.policy_verdict.v1";
pub const MODEL_PROFILE_SCHEMA: &str = "pi.runtime.model_profile.v1";

pub type Metadata = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    #[default]
    Pending,
    Ready,
    Running,
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    #[default]
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOutcome {
    #[default]
    Allow,
    Review,
    Block,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfile {
    pub schema: String,
    pub profile_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_parallel_tool_use: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl ModelProfile {
    #[must_use]
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        let provider_id = provider_id.into();
        let model_id = model_id.into();
        Self {
            schema: MODEL_PROFILE_SCHEMA.to_string(),
            profile_id: new_prefixed_id("model"),
            display_name: format!("{provider_id}/{model_id}"),
            provider_id,
            model_id,
            temperature: None,
            max_output_tokens: None,
            context_window: None,
            supports_tools: false,
            supports_parallel_tool_use: false,
            supports_reasoning: false,
            metadata: Metadata::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, MODEL_PROFILE_SCHEMA, "model profile")?;
        validate_required("profile_id", &self.profile_id)?;
        validate_required("provider_id", &self.provider_id)?;
        validate_required("model_id", &self.model_id)?;
        validate_required("display_name", &self.display_name)?;
        if let Some(temperature) = self.temperature {
            if !(0.0..=2.0).contains(&temperature) {
                return Err(Error::validation(format!(
                    "temperature must be between 0.0 and 2.0, got {temperature}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyVerdict {
    pub schema: String,
    pub verdict_id: String,
    pub policy_name: String,
    pub outcome: PolicyOutcome,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocking_rules: Vec<String>,
    pub evaluated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl PolicyVerdict {
    #[must_use]
    pub fn new(
        policy_name: impl Into<String>,
        outcome: PolicyOutcome,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            schema: POLICY_VERDICT_SCHEMA.to_string(),
            verdict_id: new_prefixed_id("policy"),
            policy_name: policy_name.into(),
            outcome,
            reason: reason.into(),
            blocking_rules: Vec::new(),
            evaluated_at: Utc::now(),
            metadata: Metadata::new(),
        }
    }

    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        matches!(self.outcome, PolicyOutcome::Block)
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, POLICY_VERDICT_SCHEMA, "policy verdict")?;
        validate_required("verdict_id", &self.verdict_id)?;
        validate_required("policy_name", &self.policy_name)?;
        validate_required("reason", &self.reason)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRuntime {
    pub schema: String,
    pub state: TaskState,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl TaskRuntime {
    #[must_use]
    pub fn new(state: TaskState) -> Self {
        let now = Utc::now();
        Self {
            schema: TASK_RUNTIME_SCHEMA.to_string(),
            state,
            attempt: 0,
            lease_owner: None,
            started_at: None,
            updated_at: now,
            completed_at: None,
            last_error: None,
            metadata: Metadata::new(),
        }
    }

    #[must_use]
    pub fn mark_running(mut self, owner: impl Into<String>) -> Self {
        let now = Utc::now();
        self.state = TaskState::Running;
        self.lease_owner = Some(owner.into());
        self.started_at.get_or_insert(now);
        self.updated_at = now;
        self.completed_at = None;
        self
    }

    #[must_use]
    pub fn mark_terminal(mut self, state: TaskState, error: Option<String>) -> Self {
        let now = Utc::now();
        self.state = state;
        self.updated_at = now;
        self.completed_at = Some(now);
        self.last_error = error;
        self
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, TASK_RUNTIME_SCHEMA, "task runtime")?;
        if let (Some(started_at), Some(completed_at)) = (self.started_at, self.completed_at) {
            if completed_at < started_at {
                return Err(Error::validation(
                    "task runtime completed_at must be >= started_at",
                ));
            }
        }
        if self.state.is_terminal() && self.completed_at.is_none() {
            return Err(Error::validation(
                "terminal task runtime must include completed_at",
            ));
        }
        if !self.state.is_terminal() && self.completed_at.is_some() {
            return Err(Error::validation(
                "non-terminal task runtime cannot include completed_at",
            ));
        }
        Ok(())
    }
}

impl Default for TaskRuntime {
    fn default() -> Self {
        Self::new(TaskState::Pending)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanArtifact {
    pub schema: String,
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub label: String,
    pub kind: String,
    pub storage_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_sha256: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl PlanArtifact {
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        kind: impl Into<String>,
        storage_uri: impl Into<String>,
    ) -> Self {
        Self {
            schema: PLAN_ARTIFACT_SCHEMA.to_string(),
            artifact_id: new_prefixed_id("artifact"),
            task_id: None,
            job_id: None,
            label: label.into(),
            kind: kind.into(),
            storage_uri: storage_uri.into(),
            media_type: None,
            byte_length: None,
            checksum_sha256: None,
            created_at: Utc::now(),
            metadata: Metadata::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, PLAN_ARTIFACT_SCHEMA, "plan artifact")?;
        validate_required("artifact_id", &self.artifact_id)?;
        validate_required("label", &self.label)?;
        validate_required("kind", &self.kind)?;
        validate_required("storage_uri", &self.storage_uri)?;
        if let Some(checksum) = &self.checksum_sha256 {
            validate_sha256("checksum_sha256", checksum)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub schema: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub name: String,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl JobRecord {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema: JOB_RECORD_SCHEMA.to_string(),
            job_id: new_prefixed_id("job"),
            task_id: None,
            name: name.into(),
            status: JobStatus::Queued,
            command: None,
            exit_code: None,
            started_at: None,
            finished_at: None,
            artifact_ids: Vec::new(),
            attempts: 0,
            metadata: Metadata::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, JOB_RECORD_SCHEMA, "job record")?;
        validate_required("job_id", &self.job_id)?;
        validate_required("name", &self.name)?;
        if let (Some(started_at), Some(finished_at)) = (self.started_at, self.finished_at) {
            if finished_at < started_at {
                return Err(Error::validation(
                    "job record finished_at must be >= started_at",
                ));
            }
        }
        if self.status.is_terminal() && self.finished_at.is_none() {
            return Err(Error::validation(
                "terminal job record must include finished_at",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskNode {
    pub schema: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    pub title: String,
    pub instructions: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_ids: Vec<String>,
    pub runtime: TaskRuntime,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl TaskNode {
    #[must_use]
    pub fn new(title: impl Into<String>, instructions: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            schema: TASK_NODE_SCHEMA.to_string(),
            task_id: new_prefixed_id("task"),
            parent_task_id: None,
            title: title.into(),
            instructions: instructions.into(),
            dependencies: Vec::new(),
            child_task_ids: Vec::new(),
            artifact_ids: Vec::new(),
            runtime: TaskRuntime::default(),
            created_at: now,
            updated_at: now,
            metadata: Metadata::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, TASK_NODE_SCHEMA, "task node")?;
        validate_required("task_id", &self.task_id)?;
        validate_required("title", &self.title)?;
        validate_required("instructions", &self.instructions)?;
        self.runtime.validate()?;
        if self.updated_at < self.created_at {
            return Err(Error::validation(
                "task node updated_at must be >= created_at",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSpec {
    pub schema: String,
    pub run_id: String,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_task_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub model_profile: ModelProfile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl RunSpec {
    #[must_use]
    pub fn new(objective: impl Into<String>, model_profile: ModelProfile) -> Self {
        Self {
            schema: RUN_SPEC_SCHEMA.to_string(),
            run_id: new_prefixed_id("run"),
            objective: objective.into(),
            requested_by: None,
            root_task_id: None,
            created_at: Utc::now(),
            model_profile,
            tags: Vec::new(),
            metadata: Metadata::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, RUN_SPEC_SCHEMA, "run spec")?;
        validate_required("run_id", &self.run_id)?;
        validate_required("objective", &self.objective)?;
        if let Some(root_task_id) = &self.root_task_id {
            validate_required("root_task_id", root_task_id)?;
        }
        self.model_profile.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub schema: String,
    pub snapshot_id: String,
    pub run_id: String,
    pub spec: RunSpec,
    pub active_model_profile: ModelProfile,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tasks: BTreeMap<String, TaskNode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub jobs: BTreeMap<String, JobRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, PlanArtifact>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub policy_verdicts: BTreeMap<String, PolicyVerdict>,
    pub last_event_sequence: u64,
    pub captured_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl RunSnapshot {
    #[must_use]
    pub fn new(spec: RunSpec) -> Self {
        Self {
            schema: RUN_SNAPSHOT_SCHEMA.to_string(),
            snapshot_id: new_prefixed_id("snapshot"),
            run_id: spec.run_id.clone(),
            active_model_profile: spec.model_profile.clone(),
            spec,
            tasks: BTreeMap::new(),
            jobs: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            policy_verdicts: BTreeMap::new(),
            last_event_sequence: 0,
            captured_at: Utc::now(),
            metadata: Metadata::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_schema(&self.schema, RUN_SNAPSHOT_SCHEMA, "run snapshot")?;
        validate_required("snapshot_id", &self.snapshot_id)?;
        validate_required("run_id", &self.run_id)?;
        self.spec.validate()?;
        self.active_model_profile.validate()?;
        if self.spec.run_id != self.run_id {
            return Err(Error::validation(
                "run snapshot run_id must match contained run spec",
            ));
        }
        for task in self.tasks.values() {
            task.validate()?;
        }
        for job in self.jobs.values() {
            job.validate()?;
        }
        for artifact in self.artifacts.values() {
            artifact.validate()?;
        }
        for verdict in self.policy_verdicts.values() {
            verdict.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn checkpoint_clone(&self) -> Self {
        let mut clone = self.clone();
        clone.snapshot_id = new_prefixed_id("snapshot");
        clone.captured_at = Utc::now();
        clone
    }

    pub fn upsert_task(&mut self, task: TaskNode) -> Result<()> {
        task.validate()?;
        self.tasks.insert(task.task_id.clone(), task);
        self.captured_at = Utc::now();
        Ok(())
    }

    pub fn upsert_job(&mut self, job: JobRecord) -> Result<()> {
        job.validate()?;
        self.jobs.insert(job.job_id.clone(), job);
        self.captured_at = Utc::now();
        Ok(())
    }

    pub fn upsert_artifact(&mut self, artifact: PlanArtifact) -> Result<()> {
        artifact.validate()?;
        self.artifacts
            .insert(artifact.artifact_id.clone(), artifact);
        self.captured_at = Utc::now();
        Ok(())
    }

    pub fn upsert_policy_verdict(&mut self, verdict: PolicyVerdict) -> Result<()> {
        verdict.validate()?;
        self.policy_verdicts
            .insert(verdict.verdict_id.clone(), verdict);
        self.captured_at = Utc::now();
        Ok(())
    }
}

fn validate_required(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::validation(format!("{field} cannot be empty")));
    }
    Ok(())
}

fn validate_schema(value: &str, expected: &str, type_name: &str) -> Result<()> {
    if value != expected {
        return Err(Error::validation(format!(
            "{type_name} schema mismatch: expected {expected}, got {value}"
        )));
    }
    Ok(())
}

fn validate_sha256(field: &str, value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::validation(format!(
            "{field} must be a 64-character hex SHA-256 digest"
        )));
    }
    Ok(())
}

fn new_prefixed_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_snapshot_round_trips_with_nested_runtime_types() {
        let mut profile = ModelProfile::new("openai", "gpt-5");
        profile.temperature = Some(0.2);
        profile.supports_tools = true;

        let spec = RunSpec::new("Implement runtime persistence", profile.clone());
        let mut snapshot = RunSnapshot::new(spec);

        let mut task = TaskNode::new("Create event log", "Write append-only event frames");
        task.runtime = TaskRuntime::new(TaskState::Ready);
        snapshot.upsert_task(task.clone()).expect("task");

        let mut job = JobRecord::new("cargo test runtime");
        job.status = JobStatus::Running;
        job.started_at = Some(Utc::now());
        snapshot.upsert_job(job.clone()).expect("job");

        let artifact = PlanArtifact::new(
            "runtime-events",
            "jsonl",
            "file:///tmp/runtime/events.jsonl",
        );
        snapshot
            .upsert_artifact(artifact.clone())
            .expect("artifact");

        let verdict = PolicyVerdict::new(
            "runtime.persistence",
            PolicyOutcome::Allow,
            "local disk writes allowed",
        );
        snapshot
            .upsert_policy_verdict(verdict.clone())
            .expect("verdict");

        snapshot.validate().expect("snapshot valid");

        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        let decoded: RunSnapshot = serde_json::from_str(&json).expect("deserialize snapshot");

        assert_eq!(decoded.run_id, snapshot.run_id);
        assert_eq!(decoded.active_model_profile, profile);
        assert_eq!(decoded.tasks.len(), 1);
        assert_eq!(decoded.jobs.len(), 1);
        assert_eq!(decoded.artifacts.len(), 1);
        assert_eq!(decoded.policy_verdicts.len(), 1);
        assert_eq!(decoded.tasks[&task.task_id], task);
        assert_eq!(decoded.jobs[&job.job_id], job);
        assert_eq!(decoded.artifacts[&artifact.artifact_id], artifact);
        assert_eq!(decoded.policy_verdicts[&verdict.verdict_id], verdict);
    }

    #[test]
    fn validation_rejects_inconsistent_terminal_runtime() {
        let runtime = TaskRuntime::new(TaskState::Succeeded);
        let err = runtime.validate().expect_err("terminal runtime must fail");
        assert!(err.to_string().contains("completed_at"));
    }

    #[test]
    fn plan_artifact_rejects_invalid_sha256() {
        let mut artifact = PlanArtifact::new("plan", "markdown", "file:///tmp/plan.md");
        artifact.checksum_sha256 = Some("abc123".to_string());

        let err = artifact.validate().expect_err("invalid checksum");
        assert!(err.to_string().contains("SHA-256"));
    }
}
