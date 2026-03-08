use crate::config::Config;
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTier {
    Inline,
    Wave,
    Hierarchical,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RunStatus {
    pub run_id: String,
    pub objective: String,
    pub selected_tier: ExecutionTier,
    pub lifecycle: RunLifecycle,
    #[serde(default)]
    pub task_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_subrun_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_wave: Option<WaveStatus>,
    #[serde(default)]
    pub task_counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub task_reports: BTreeMap<String, TaskReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_run_verify_summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RunStatus {
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
            task_ids: Vec::new(),
            active_subrun_id: None,
            active_wave: None,
            task_counts: BTreeMap::new(),
            task_reports: BTreeMap::new(),
            latest_run_verify_summary: None,
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
}

#[derive(Debug, Clone)]
pub struct RunStore {
    root: PathBuf,
}

impl RunStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn from_global_dir() -> Self {
        Self::new(Config::global_dir().join("orchestration").join("runs"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load(&self, run_id: &str) -> Result<RunStatus> {
        let path = self.run_path(run_id);
        let bytes = fs::read(&path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::session(format!("orchestration run not found: {run_id}"))
            } else {
                Error::Io(Box::new(err))
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|err| Error::Json(Box::new(err)))
    }

    pub fn save(&self, status: &RunStatus) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(|err| Error::Io(Box::new(err)))?;
        let encoded =
            serde_json::to_vec_pretty(status).map_err(|err| Error::Json(Box::new(err)))?;
        fs::write(self.run_path(&status.run_id), encoded).map_err(|err| Error::Io(Box::new(err)))
    }

    fn run_path(&self, run_id: &str) -> PathBuf {
        self.root.join(format!("{run_id}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn run_store_round_trip() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp_dir.path().to_path_buf());
        let mut run = RunStatus::new("run-2", "Round trip", ExecutionTier::Inline);
        run.lifecycle = RunLifecycle::Running;
        run.latest_run_verify_summary = Some("ok".to_string());

        store.save(&run).expect("save");
        let loaded = store.load("run-2").expect("load");

        assert_eq!(loaded.run_id, "run-2");
        assert_eq!(loaded.lifecycle, RunLifecycle::Running);
        assert_eq!(loaded.latest_run_verify_summary.as_deref(), Some("ok"));
    }
}
