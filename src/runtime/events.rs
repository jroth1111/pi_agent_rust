use super::types::{
    JobRecord, Metadata, ModelProfile, PlanArtifact, PolicyVerdict, RunSpec, TaskNode, TaskRuntime,
};
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const RUNTIME_EVENT_SCHEMA: &str = "pi.runtime.event.v1";
const EVENT_LOG_FILE_NAME: &str = "events.jsonl";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEvent {
    pub schema: String,
    pub event_id: String,
    pub run_id: String,
    pub sequence: u64,
    pub recorded_at: DateTime<Utc>,
    pub kind: RuntimeEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "eventType", rename_all = "snake_case")]
pub enum RuntimeEventKind {
    RunCreated {
        spec: RunSpec,
    },
    ModelProfileUpdated {
        profile: ModelProfile,
    },
    TaskNodeUpserted {
        task: TaskNode,
    },
    TaskRuntimeUpdated {
        task_id: String,
        runtime: TaskRuntime,
    },
    JobRecorded {
        job: JobRecord,
    },
    ArtifactRecorded {
        artifact: PlanArtifact,
    },
    PolicyVerdictRecorded {
        verdict: PolicyVerdict,
    },
}

impl RuntimeEventKind {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::RunCreated { spec } => spec.validate(),
            Self::ModelProfileUpdated { profile } => profile.validate(),
            Self::TaskNodeUpserted { task } => task.validate(),
            Self::TaskRuntimeUpdated { task_id, runtime } => {
                if task_id.trim().is_empty() {
                    return Err(Error::validation("task_id cannot be empty"));
                }
                runtime.validate()
            }
            Self::JobRecorded { job } => job.validate(),
            Self::ArtifactRecorded { artifact } => artifact.validate(),
            Self::PolicyVerdictRecorded { verdict } => verdict.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEventDraft {
    pub run_id: String,
    pub kind: RuntimeEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl RuntimeEventDraft {
    #[must_use]
    pub fn new(run_id: impl Into<String>, kind: RuntimeEventKind) -> Self {
        Self {
            run_id: run_id.into(),
            kind,
            actor: None,
            correlation_id: None,
            metadata: Metadata::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.run_id.trim().is_empty() {
            return Err(Error::validation("run_id cannot be empty"));
        }
        self.kind.validate()
    }

    pub fn into_event(self, sequence: u64) -> Result<RuntimeEvent> {
        self.validate()?;
        let event = RuntimeEvent {
            schema: RUNTIME_EVENT_SCHEMA.to_string(),
            event_id: format!("event-{}", Uuid::new_v4().simple()),
            run_id: self.run_id,
            sequence,
            recorded_at: Utc::now(),
            kind: self.kind,
            actor: self.actor,
            correlation_id: self.correlation_id,
            metadata: self.metadata,
        };
        event.validate()?;
        Ok(event)
    }
}

impl RuntimeEvent {
    pub fn validate(&self) -> Result<()> {
        if self.schema != RUNTIME_EVENT_SCHEMA {
            return Err(Error::validation(format!(
                "runtime event schema mismatch: expected {RUNTIME_EVENT_SCHEMA}, got {}",
                self.schema
            )));
        }
        if self.event_id.trim().is_empty() {
            return Err(Error::validation("event_id cannot be empty"));
        }
        if self.run_id.trim().is_empty() {
            return Err(Error::validation("run_id cannot be empty"));
        }
        if self.sequence == 0 {
            return Err(Error::validation("event sequence must be >= 1"));
        }
        self.kind.validate()
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeEventLog {
    root: PathBuf,
    log_path: PathBuf,
}

impl RuntimeEventLog {
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(io_error)?;
        let log_path = root.join(EVENT_LOG_FILE_NAME);
        if !log_path.exists() {
            File::create(&log_path).map_err(io_error)?;
        }
        Ok(Self { root, log_path })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn append(&self, draft: RuntimeEventDraft) -> Result<RuntimeEvent> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.log_path)
            .map_err(io_error)?;
        file.lock_exclusive().map_err(io_error)?;

        let result = (|| {
            let next_sequence = read_last_sequence(&mut file)? + 1;
            let event = draft.into_event(next_sequence)?;
            let serialized = serde_json::to_string(&event).map_err(json_error)?;
            file.seek(SeekFrom::End(0)).map_err(io_error)?;
            file.write_all(serialized.as_bytes()).map_err(io_error)?;
            file.write_all(b"\n").map_err(io_error)?;
            file.sync_data().map_err(io_error)?;
            Ok(event)
        })();

        let unlock_result = file.unlock();
        match (result, unlock_result) {
            (Ok(event), Ok(())) => Ok(event),
            (Err(err), Ok(())) => Err(err),
            (Ok(_), Err(err)) => Err(io_error(err)),
            (Err(err), Err(_)) => Err(err),
        }
    }

    pub fn read_all(&self) -> Result<Vec<RuntimeEvent>> {
        self.read_since(1)
    }

    pub fn read_since(&self, sequence_inclusive: u64) -> Result<Vec<RuntimeEvent>> {
        let file = self.open_read_file()?;
        file.lock_shared().map_err(io_error)?;

        let result = (|| {
            let reader = BufReader::new(&file);
            let mut events = Vec::new();
            let mut last_sequence = 0_u64;
            for (line_no, line) in reader.lines().enumerate() {
                let line = line.map_err(io_error)?;
                if line.trim().is_empty() {
                    continue;
                }
                let event: RuntimeEvent = serde_json::from_str(&line).map_err(|error| {
                    Error::session(format!(
                        "failed to parse runtime event log line {}: {error}",
                        line_no + 1
                    ))
                })?;
                event.validate()?;
                if event.sequence <= last_sequence {
                    return Err(Error::session(format!(
                        "runtime event sequence regression at line {}: {} after {}",
                        line_no + 1,
                        event.sequence,
                        last_sequence
                    )));
                }
                last_sequence = event.sequence;
                if event.sequence >= sequence_inclusive {
                    events.push(event);
                }
            }
            Ok(events)
        })();

        let unlock_result = file.unlock();
        match (result, unlock_result) {
            (Ok(events), Ok(())) => Ok(events),
            (Err(err), Ok(())) => Err(err),
            (Ok(_), Err(err)) => Err(io_error(err)),
            (Err(err), Err(_)) => Err(err),
        }
    }

    pub fn latest_sequence(&self) -> Result<u64> {
        let mut file = self.open_read_file()?;
        file.lock_shared().map_err(io_error)?;
        let result = read_last_sequence(&mut file);
        let unlock_result = file.unlock();
        match (result, unlock_result) {
            (Ok(sequence), Ok(())) => Ok(sequence),
            (Err(err), Ok(())) => Err(err),
            (Ok(_), Err(err)) => Err(io_error(err)),
            (Err(err), Err(_)) => Err(err),
        }
    }

    fn open_read_file(&self) -> Result<File> {
        if !self.log_path.exists() {
            File::create(&self.log_path).map_err(io_error)?;
        }
        OpenOptions::new()
            .read(true)
            .open(&self.log_path)
            .map_err(io_error)
    }
}

fn read_last_sequence(file: &mut File) -> Result<u64> {
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(io_error)?;

    let mut last_sequence = 0_u64;
    for (line_no, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: RuntimeEvent = serde_json::from_str(line).map_err(|error| {
            Error::session(format!(
                "failed to parse runtime event log line {}: {error}",
                line_no + 1
            ))
        })?;
        event.validate()?;
        if event.sequence <= last_sequence {
            return Err(Error::session(format!(
                "runtime event sequence regression at line {}: {} after {}",
                line_no + 1,
                event.sequence,
                last_sequence
            )));
        }
        last_sequence = event.sequence;
    }

    Ok(last_sequence)
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
    use crate::runtime::types::{ModelProfile, RunSpec, TaskState};
    use tempfile::TempDir;

    #[test]
    fn event_log_assigns_monotonic_sequences() {
        let tmp = TempDir::new().expect("tempdir");
        let log = RuntimeEventLog::create(tmp.path()).expect("create log");

        let spec = RunSpec::new(
            "Build runtime store",
            ModelProfile::new("anthropic", "claude-sonnet"),
        );
        let run_id = spec.run_id.clone();

        let first = log
            .append(RuntimeEventDraft::new(
                run_id.clone(),
                RuntimeEventKind::RunCreated { spec },
            ))
            .expect("append first");
        let second = log
            .append(RuntimeEventDraft::new(
                run_id.clone(),
                RuntimeEventKind::TaskRuntimeUpdated {
                    task_id: "task-1".to_string(),
                    runtime: TaskRuntime::new(TaskState::Ready),
                },
            ))
            .expect("append second");

        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(log.latest_sequence().expect("latest"), 2);
    }

    #[test]
    fn event_log_reads_filtered_tail() {
        let tmp = TempDir::new().expect("tempdir");
        let log = RuntimeEventLog::create(tmp.path()).expect("create log");

        let spec = RunSpec::new("Persist snapshots", ModelProfile::new("openai", "gpt-5"));
        let run_id = spec.run_id.clone();
        let task = TaskNode::new("Task A", "do work");

        log.append(RuntimeEventDraft::new(
            run_id.clone(),
            RuntimeEventKind::RunCreated { spec },
        ))
        .expect("append run");
        log.append(RuntimeEventDraft::new(
            run_id,
            RuntimeEventKind::TaskNodeUpserted { task: task.clone() },
        ))
        .expect("append task");

        let tail = log.read_since(2).expect("tail");
        assert_eq!(tail.len(), 1);
        assert!(matches!(
            tail[0].kind,
            RuntimeEventKind::TaskNodeUpserted { .. }
        ));
    }
}
