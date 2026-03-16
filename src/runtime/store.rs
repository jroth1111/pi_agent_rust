use crate::config::Config;
use crate::error::{Error, Result};
use crate::runtime::events::RuntimeEvent;
use crate::runtime::types::RunSnapshot;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn from_global_dir() -> Self {
        Self::new(Config::global_dir().join("runtime").join("runs"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn save_snapshot(&self, snapshot: &RunSnapshot) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(|err| Error::Io(Box::new(err)))?;
        let encoded =
            serde_json::to_vec_pretty(snapshot).map_err(|err| Error::Json(Box::new(err)))?;
        fs::write(self.snapshot_path(&snapshot.spec.run_id), encoded)
            .map_err(|err| Error::Io(Box::new(err)))
    }

    pub fn load_snapshot(&self, run_id: &str) -> Result<RunSnapshot> {
        let bytes = fs::read(self.snapshot_path(run_id)).map_err(|err| Error::Io(Box::new(err)))?;
        serde_json::from_slice(&bytes).map_err(|err| Error::Json(Box::new(err)))
    }

    pub fn append_event(&self, event: &RuntimeEvent) -> Result<()> {
        fs::create_dir_all(&self.root).map_err(|err| Error::Io(Box::new(err)))?;
        let encoded = serde_json::to_string(event).map_err(|err| Error::Json(Box::new(err)))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path(&event.run_id))
            .map_err(|err| Error::Io(Box::new(err)))?;
        file.write_all(encoded.as_bytes())
            .map_err(|err| Error::Io(Box::new(err)))?;
        file.write_all(b"\n")
            .map_err(|err| Error::Io(Box::new(err)))?;
        Ok(())
    }

    pub fn append_events(&self, events: &[RuntimeEvent]) -> Result<()> {
        for event in events {
            self.append_event(event)?;
        }
        Ok(())
    }

    pub fn load_events(&self, run_id: &str) -> Result<Vec<RuntimeEvent>> {
        let path = self.events_path(run_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|err| Error::Io(Box::new(err)))?;
        let reader = BufReader::new(file);
        reader
            .lines()
            .filter(|line| line.as_ref().is_ok_and(|line| !line.trim().is_empty()))
            .map(|line| {
                let line = line.map_err(|err| Error::Io(Box::new(err)))?;
                serde_json::from_str(&line).map_err(|err| Error::Json(Box::new(err)))
            })
            .collect()
    }

    pub fn exists(&self, run_id: &str) -> bool {
        self.snapshot_path(run_id).exists()
    }

    pub fn list_run_ids(&self) -> Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        let mut run_ids = fs::read_dir(&self.root)
            .map_err(|err| Error::Io(Box::new(err)))?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name();
                let name = name.to_str()?;
                name.strip_suffix(".snapshot.json").map(str::to_string)
            })
            .collect::<Vec<_>>();
        run_ids.sort();
        Ok(run_ids)
    }

    pub fn find_run_ids_by_task(&self, task_id: &str) -> Result<Vec<String>> {
        let mut run_ids = Vec::new();
        for run_id in self.list_run_ids()? {
            let snapshot = self.load_snapshot(&run_id)?;
            if snapshot.tasks.contains_key(task_id) {
                run_ids.push(run_id);
            }
        }
        Ok(run_ids)
    }

    fn snapshot_path(&self, run_id: &str) -> PathBuf {
        self.root.join(format!("{run_id}.snapshot.json"))
    }

    fn events_path(&self, run_id: &str) -> PathBuf {
        self.root.join(format!("{run_id}.events.jsonl"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::events::RuntimeEventKind;
    use crate::runtime::types::{
        AutonomyLevel, RunBudgets, RunConstraints, RunSpec, TaskConstraints, TaskNode, TaskSpec,
        VerifySpec,
    };
    use chrono::Utc;
    use tempfile::tempdir;

    fn sample_snapshot() -> RunSnapshot {
        RunSnapshot::new(RunSpec {
            run_id: "run-1".to_string(),
            objective: "runtime".to_string(),
            root_workspace: PathBuf::from("/tmp/pi"),
            policy_profile: "default".to_string(),
            model_profile: "default".to_string(),
            run_verify_command: Some("cargo test".to_string()),
            run_verify_timeout_sec: Some(60),
            budgets: RunBudgets::default(),
            constraints: RunConstraints::default(),
            created_at: Utc::now(),
        })
    }

    #[test]
    fn store_round_trips_snapshot_and_events() {
        let dir = tempdir().expect("tempdir");
        let store = RuntimeStore::new(dir.path().to_path_buf());
        let snapshot = sample_snapshot();
        store.save_snapshot(&snapshot).expect("save snapshot");
        let loaded = store
            .load_snapshot(&snapshot.spec.run_id)
            .expect("load snapshot");
        assert_eq!(loaded.spec.run_id, snapshot.spec.run_id);

        let event = RuntimeEvent::new("run-1", RuntimeEventKind::RunCreated);
        store.append_event(&event).expect("append event");
        let events = store.load_events("run-1").expect("load events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label(), "run_created");
    }

    #[test]
    fn store_finds_runs_by_task_id_from_snapshots() {
        let dir = tempdir().expect("tempdir");
        let store = RuntimeStore::new(dir.path().to_path_buf());

        let mut run_a = sample_snapshot();
        run_a.spec.run_id = "run-a".to_string();
        run_a.tasks.insert(
            "task-a".to_string(),
            TaskNode::new(TaskSpec {
                task_id: "task-a".to_string(),
                title: "Task A".to_string(),
                objective: "Do task a".to_string(),
                parent_goal_trace_id: None,
                planned_touches: Vec::new(),
                input_snapshot: None,
                max_attempts: 1,
                enforce_symbol_drift_check: false,
                verify: VerifySpec {
                    command: "cargo test".to_string(),
                    timeout_sec: 60,
                    acceptance_ids: Vec::new(),
                },
                autonomy: AutonomyLevel::Guarded,
                constraints: TaskConstraints::default(),
            }),
        );
        store.save_snapshot(&run_a).expect("save run a");

        let mut run_b = sample_snapshot();
        run_b.spec.run_id = "run-b".to_string();
        run_b.tasks.insert(
            "task-b".to_string(),
            TaskNode::new(TaskSpec {
                task_id: "task-b".to_string(),
                title: "Task B".to_string(),
                objective: "Do task b".to_string(),
                parent_goal_trace_id: None,
                planned_touches: Vec::new(),
                input_snapshot: None,
                max_attempts: 1,
                enforce_symbol_drift_check: false,
                verify: VerifySpec {
                    command: "cargo test".to_string(),
                    timeout_sec: 60,
                    acceptance_ids: Vec::new(),
                },
                autonomy: AutonomyLevel::Guarded,
                constraints: TaskConstraints::default(),
            }),
        );
        store.save_snapshot(&run_b).expect("save run b");

        let run_ids = store
            .find_run_ids_by_task("task-b")
            .expect("find run ids by task");
        assert_eq!(run_ids, vec!["run-b".to_string()]);
    }
}
