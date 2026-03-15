use super::events::{RuntimeEvent, RuntimeEventDraft, RuntimeEventKind, RuntimeEventLog};
use super::types::{RunSnapshot, RunSpec, TaskNode, TaskRuntime};
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const RUNTIME_STORE_MANIFEST_SCHEMA: &str = "pi.runtime.store_manifest.v1";
const EVENTS_DIR: &str = "events";
const SNAPSHOTS_DIR: &str = "snapshots";
const CHECKPOINTS_DIR: &str = "checkpoints";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStoreManifest {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_snapshot_id: Option<String>,
    #[serde(default)]
    pub latest_event_sequence: u64,
    #[serde(default)]
    pub snapshot_count: u64,
}

impl RuntimeStoreManifest {
    #[must_use]
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            schema: RUNTIME_STORE_MANIFEST_SCHEMA.to_string(),
            run_id: None,
            created_at: now,
            updated_at: now,
            latest_snapshot_id: None,
            latest_event_sequence: 0,
            snapshot_count: 0,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != RUNTIME_STORE_MANIFEST_SCHEMA {
            return Err(Error::validation(format!(
                "runtime store manifest schema mismatch: expected {RUNTIME_STORE_MANIFEST_SCHEMA}, got {}",
                self.schema
            )));
        }
        Ok(())
    }
}

impl Default for RuntimeStoreManifest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshotStore {
    root: PathBuf,
    snapshots_dir: PathBuf,
    manifest_path: PathBuf,
}

impl RuntimeSnapshotStore {
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let snapshots_dir = root.join(CHECKPOINTS_DIR);
        fs::create_dir_all(&snapshots_dir).map_err(io_error)?;
        let manifest_path = root.join(MANIFEST_FILE);
        let store = Self {
            root,
            snapshots_dir,
            manifest_path,
        };
        if !store.manifest_path.exists() {
            store.write_manifest(&RuntimeStoreManifest::new())?;
        }
        Ok(store)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load_manifest(&self) -> Result<RuntimeStoreManifest> {
        let bytes = fs::read(&self.manifest_path).map_err(io_error)?;
        let manifest: RuntimeStoreManifest = serde_json::from_slice(&bytes).map_err(json_error)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn save_snapshot(&self, snapshot: &RunSnapshot) -> Result<RuntimeStoreManifest> {
        snapshot.validate()?;

        let snapshot_path = self.snapshot_path(&snapshot.snapshot_id);
        let tmp_path = snapshot_path.with_extension("json.tmp");
        write_json_file(&tmp_path, snapshot)?;
        fs::rename(&tmp_path, &snapshot_path).map_err(io_error)?;

        let mut manifest = self.load_manifest().unwrap_or_default();
        if manifest.run_id.as_deref().is_none() {
            manifest.run_id = Some(snapshot.run_id.clone());
        } else if manifest.run_id.as_deref() != Some(snapshot.run_id.as_str()) {
            return Err(Error::session(format!(
                "snapshot run_id {} does not match manifest run_id {}",
                snapshot.run_id,
                manifest.run_id.as_deref().unwrap_or_default()
            )));
        }
        manifest.latest_snapshot_id = Some(snapshot.snapshot_id.clone());
        manifest.latest_event_sequence = snapshot.last_event_sequence;
        manifest.snapshot_count += 1;
        manifest.updated_at = Utc::now();
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn load_latest_snapshot(&self) -> Result<Option<RunSnapshot>> {
        let manifest = self.load_manifest()?;
        let Some(snapshot_id) = manifest.latest_snapshot_id else {
            return Ok(None);
        };
        self.load_snapshot(&snapshot_id)
    }

    pub fn load_snapshot(&self, snapshot_id: &str) -> Result<Option<RunSnapshot>> {
        if snapshot_id.trim().is_empty() {
            return Err(Error::validation("snapshot_id cannot be empty"));
        }
        let path = self.snapshot_path(snapshot_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path).map_err(io_error)?;
        let snapshot: RunSnapshot = serde_json::from_slice(&bytes).map_err(json_error)?;
        snapshot.validate()?;
        Ok(Some(snapshot))
    }

    fn snapshot_path(&self, snapshot_id: &str) -> PathBuf {
        self.snapshots_dir.join(format!("{snapshot_id}.json"))
    }

    fn write_manifest(&self, manifest: &RuntimeStoreManifest) -> Result<()> {
        manifest.validate()?;
        let tmp_path = self.manifest_path.with_extension("json.tmp");
        write_json_file(&tmp_path, manifest)?;
        fs::rename(&tmp_path, &self.manifest_path).map_err(io_error)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeStore {
    root: PathBuf,
    pub events: RuntimeEventLog,
    pub snapshots: RuntimeSnapshotStore,
}

impl RuntimeStore {
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(io_error)?;
        let events = RuntimeEventLog::create(root.join(EVENTS_DIR))?;
        let snapshots = RuntimeSnapshotStore::create(root.join(SNAPSHOTS_DIR))?;
        Ok(Self {
            root,
            events,
            snapshots,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_run(&self, spec: RunSpec, actor: Option<String>) -> Result<RuntimeEvent> {
        spec.validate()?;
        if self.events.latest_sequence()? != 0 {
            return Err(Error::session(
                "runtime store is already initialized for a run",
            ));
        }
        let mut draft =
            RuntimeEventDraft::new(spec.run_id.clone(), RuntimeEventKind::RunCreated { spec });
        draft.actor = actor;
        self.events.append(draft)
    }

    pub fn record(&self, draft: RuntimeEventDraft) -> Result<RuntimeEvent> {
        let latest_sequence = self.events.latest_sequence()?;
        if latest_sequence == 0 {
            if !matches!(draft.kind, RuntimeEventKind::RunCreated { .. }) {
                return Err(Error::session(
                    "runtime store must begin with a RunCreated event",
                ));
            }
        } else if matches!(draft.kind, RuntimeEventKind::RunCreated { .. }) {
            return Err(Error::session(
                "runtime store cannot record multiple RunCreated events",
            ));
        }
        self.events.append(draft)
    }

    pub fn materialize_snapshot(&self) -> Result<Option<RunSnapshot>> {
        let mut snapshot = self.snapshots.load_latest_snapshot()?;
        let next_sequence = snapshot
            .as_ref()
            .map_or(1, |snapshot| snapshot.last_event_sequence.saturating_add(1));
        let events = self.events.read_since(next_sequence)?;

        if snapshot.is_none() && events.is_empty() {
            return Ok(None);
        }

        for event in events {
            apply_event(&mut snapshot, &event)?;
        }

        Ok(snapshot)
    }

    pub fn checkpoint(&self) -> Result<Option<RunSnapshot>> {
        let snapshot = self.materialize_snapshot()?;
        if let Some(snapshot) = snapshot {
            let manifest = self.snapshots.load_manifest()?;
            if manifest.latest_snapshot_id.is_some()
                && snapshot.last_event_sequence == manifest.latest_event_sequence
            {
                return Ok(Some(snapshot));
            }

            let persisted = snapshot.checkpoint_clone();
            self.snapshots.save_snapshot(&persisted)?;
            Ok(Some(persisted))
        } else {
            Ok(None)
        }
    }
}

fn apply_event(snapshot: &mut Option<RunSnapshot>, event: &RuntimeEvent) -> Result<()> {
    event.validate()?;

    match &event.kind {
        RuntimeEventKind::RunCreated { spec } => {
            if snapshot.is_some() {
                return Err(Error::session(format!(
                    "run {} already initialized before event {}",
                    event.run_id, event.sequence
                )));
            }
            let mut next_snapshot = RunSnapshot::new(spec.clone());
            next_snapshot.last_event_sequence = event.sequence;
            next_snapshot.captured_at = event.recorded_at;
            next_snapshot.metadata = event.metadata.clone();
            *snapshot = Some(next_snapshot);
        }
        RuntimeEventKind::ModelProfileUpdated { profile } => {
            let current = require_snapshot(snapshot.as_mut(), event)?;
            current.active_model_profile = profile.clone();
            current.last_event_sequence = event.sequence;
            current.captured_at = event.recorded_at;
        }
        RuntimeEventKind::TaskNodeUpserted { task } => {
            let current = require_snapshot(snapshot.as_mut(), event)?;
            current.upsert_task(task.clone())?;
            current.last_event_sequence = event.sequence;
            current.captured_at = event.recorded_at;
        }
        RuntimeEventKind::TaskRuntimeUpdated { task_id, runtime } => {
            let current = require_snapshot(snapshot.as_mut(), event)?;
            let task = current.tasks.get_mut(task_id).ok_or_else(|| {
                Error::session(format!(
                    "cannot apply task runtime event {} for missing task {}",
                    event.sequence, task_id
                ))
            })?;
            update_task_runtime(task, runtime.clone(), event.recorded_at)?;
            current.last_event_sequence = event.sequence;
            current.captured_at = event.recorded_at;
        }
        RuntimeEventKind::JobRecorded { job } => {
            let current = require_snapshot(snapshot.as_mut(), event)?;
            current.upsert_job(job.clone())?;
            current.last_event_sequence = event.sequence;
            current.captured_at = event.recorded_at;
        }
        RuntimeEventKind::ArtifactRecorded { artifact } => {
            let current = require_snapshot(snapshot.as_mut(), event)?;
            current.upsert_artifact(artifact.clone())?;
            current.last_event_sequence = event.sequence;
            current.captured_at = event.recorded_at;
        }
        RuntimeEventKind::PolicyVerdictRecorded { verdict } => {
            let current = require_snapshot(snapshot.as_mut(), event)?;
            current.upsert_policy_verdict(verdict.clone())?;
            current.last_event_sequence = event.sequence;
            current.captured_at = event.recorded_at;
        }
    }

    Ok(())
}

fn require_snapshot<'a>(
    snapshot: Option<&'a mut RunSnapshot>,
    event: &RuntimeEvent,
) -> Result<&'a mut RunSnapshot> {
    snapshot.ok_or_else(|| {
        Error::session(format!(
            "runtime event {} ({}) arrived before RunCreated",
            event.sequence, event.run_id
        ))
    })
}

fn update_task_runtime(
    task: &mut TaskNode,
    runtime: TaskRuntime,
    recorded_at: DateTime<Utc>,
) -> Result<()> {
    runtime.validate()?;
    task.runtime = runtime;
    task.updated_at = recorded_at;
    Ok(())
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    let mut file = File::create(path).map_err(io_error)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_data().map_err(io_error)?;
    Ok(())
}

fn io_error(error: std::io::Error) -> Error {
    Error::Io(Box::new(error))
}

fn json_error(error: serde_json::Error) -> Error {
    Error::Json(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::types::{
        JobRecord, JobStatus, ModelProfile, PlanArtifact, PolicyOutcome, PolicyVerdict, TaskState,
    };
    use tempfile::TempDir;

    #[test]
    fn runtime_store_materializes_snapshot_from_event_log() {
        let tmp = TempDir::new().expect("tempdir");
        let store = RuntimeStore::create(tmp.path()).expect("store");

        let spec = RunSpec::new(
            "Implement durable runtime layer",
            ModelProfile::new("openai", "gpt-5"),
        );
        let run_id = spec.run_id.clone();
        store
            .create_run(spec, Some("agent".to_string()))
            .expect("run");

        let mut task = TaskNode::new("Plan runtime", "Define types and persistence");
        let task_id = task.task_id.clone();
        task.runtime = TaskRuntime::new(TaskState::Ready);
        store
            .record(RuntimeEventDraft::new(
                run_id.clone(),
                RuntimeEventKind::TaskNodeUpserted { task: task.clone() },
            ))
            .expect("task event");

        let runtime = TaskRuntime::new(TaskState::Running).mark_running("agent");
        store
            .record(RuntimeEventDraft::new(
                run_id.clone(),
                RuntimeEventKind::TaskRuntimeUpdated {
                    task_id: task_id.clone(),
                    runtime: runtime.clone(),
                },
            ))
            .expect("runtime event");

        let mut job = JobRecord::new("cargo test");
        job.task_id = Some(task_id.clone());
        job.status = JobStatus::Running;
        job.started_at = Some(Utc::now());
        store
            .record(RuntimeEventDraft::new(
                run_id.clone(),
                RuntimeEventKind::JobRecorded { job: job.clone() },
            ))
            .expect("job event");

        let artifact = PlanArtifact::new("snapshot", "json", "file:///tmp/runtime/snapshot.json");
        store
            .record(RuntimeEventDraft::new(
                run_id.clone(),
                RuntimeEventKind::ArtifactRecorded {
                    artifact: artifact.clone(),
                },
            ))
            .expect("artifact event");

        let verdict = PolicyVerdict::new(
            "runtime.write_policy",
            PolicyOutcome::Allow,
            "runtime store may write local state",
        );
        store
            .record(RuntimeEventDraft::new(
                run_id,
                RuntimeEventKind::PolicyVerdictRecorded {
                    verdict: verdict.clone(),
                },
            ))
            .expect("verdict event");

        let snapshot = store
            .materialize_snapshot()
            .expect("materialize")
            .expect("snapshot");

        assert_eq!(snapshot.tasks[&task_id].runtime, runtime);
        assert_eq!(snapshot.jobs[&job.job_id], job);
        assert_eq!(snapshot.artifacts[&artifact.artifact_id], artifact);
        assert_eq!(snapshot.policy_verdicts[&verdict.verdict_id], verdict);
        assert_eq!(snapshot.last_event_sequence, 6);
    }

    #[test]
    fn checkpoint_reuses_latest_snapshot_and_applies_new_events() {
        let tmp = TempDir::new().expect("tempdir");
        let store = RuntimeStore::create(tmp.path()).expect("store");

        let spec = RunSpec::new("Checkpoint runtime", ModelProfile::new("openai", "gpt-5"));
        let run_id = spec.run_id.clone();
        store.create_run(spec, None).expect("run");

        let task = TaskNode::new("Seed task", "first snapshot");
        let task_id = task.task_id.clone();
        store
            .record(RuntimeEventDraft::new(
                run_id.clone(),
                RuntimeEventKind::TaskNodeUpserted { task },
            ))
            .expect("task");

        let first = store.checkpoint().expect("checkpoint").expect("snapshot");
        assert_eq!(first.last_event_sequence, 2);

        let runtime = TaskRuntime::new(TaskState::Succeeded)
            .mark_running("agent")
            .mark_terminal(TaskState::Succeeded, None);
        store
            .record(RuntimeEventDraft::new(
                run_id,
                RuntimeEventKind::TaskRuntimeUpdated {
                    task_id: task_id.clone(),
                    runtime: runtime.clone(),
                },
            ))
            .expect("runtime");

        let second = store.checkpoint().expect("checkpoint").expect("snapshot");
        assert_eq!(second.last_event_sequence, 3);
        assert_eq!(second.tasks[&task_id].runtime, runtime);
        assert_ne!(first.snapshot_id, second.snapshot_id);

        let manifest = store.snapshots.load_manifest().expect("manifest");
        assert_eq!(manifest.latest_event_sequence, 3);
        assert_eq!(manifest.snapshot_count, 2);
    }

    #[test]
    fn runtime_store_rejects_non_bootstrap_first_event() {
        let tmp = TempDir::new().expect("tempdir");
        let store = RuntimeStore::create(tmp.path()).expect("store");

        let err = store
            .record(RuntimeEventDraft::new(
                "run-test",
                RuntimeEventKind::TaskRuntimeUpdated {
                    task_id: "task-1".to_string(),
                    runtime: TaskRuntime::new(TaskState::Ready),
                },
            ))
            .expect_err("non-bootstrap first event must fail");

        assert!(err
            .to_string()
            .contains("must begin with a RunCreated event"));
    }
}
