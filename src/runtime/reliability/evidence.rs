use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Pending,
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub evidence_id: String,
    pub task_id: String,
    pub command: String,
    pub exit_code: i32,
    pub status: EvidenceStatus,
    pub stdout_hash: Option<String>,
    pub stderr_hash: Option<String>,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_id: Option<String>,
    #[serde(rename = "ts_utc", alias = "timestamp_utc")]
    pub timestamp_utc: DateTime<Utc>,
}

impl EvidenceRecord {
    pub fn from_command_output(
        task_id: impl Into<String>,
        command: impl Into<String>,
        exit_code: i32,
        stdout: &str,
        stderr: &str,
        artifact_ids: Vec<String>,
    ) -> Self {
        Self::from_command_output_with_env(
            task_id,
            command,
            exit_code,
            stdout,
            stderr,
            artifact_ids,
            None,
        )
    }

    pub fn from_command_output_with_env(
        task_id: impl Into<String>,
        command: impl Into<String>,
        exit_code: i32,
        stdout: &str,
        stderr: &str,
        artifact_ids: Vec<String>,
        env_id: Option<String>,
    ) -> Self {
        let task_id = task_id.into();
        let command = command.into();
        let status = if exit_code == 0 {
            EvidenceStatus::Passed
        } else {
            EvidenceStatus::Failed
        };
        let stdout_hash = hash_if_not_empty(stdout);
        let stderr_hash = hash_if_not_empty(stderr);
        let now = Utc::now();
        let evidence_id = format!(
            "ev-{}-{}",
            now.timestamp_millis(),
            stable_short_hash(&format!("{task_id}|{command}|{exit_code}"))
        );

        Self {
            evidence_id,
            task_id,
            command,
            exit_code,
            status,
            stdout_hash,
            stderr_hash,
            artifact_ids,
            env_id,
            timestamp_utc: now,
        }
    }

    pub const fn is_success(&self) -> bool {
        matches!(self.status, EvidenceStatus::Passed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDigest {
    pub generated_at: DateTime<Utc>,
    pub objective: String,
    pub phase: String,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub recent_actions: Vec<String>,
    pub next_action: Option<String>,
}

impl StateDigest {
    pub fn new(objective: impl Into<String>, phase: impl Into<String>) -> Self {
        Self {
            generated_at: Utc::now(),
            objective: objective.into(),
            phase: phase.into(),
            blockers: Vec::new(),
            recent_actions: Vec::new(),
            next_action: None,
        }
    }
}

fn hash_if_not_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    Some(sha256_hex(value.as_bytes()))
}

fn stable_short_hash(value: &str) -> String {
    let full = sha256_hex(value.as_bytes());
    full[..12].to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_from_command_output_records_hashes_and_pass_status() {
        let rec = EvidenceRecord::from_command_output(
            "task-1",
            "cargo test",
            0,
            "ok output",
            "",
            vec!["task-1/verify/x.bin".to_string()],
        );
        assert!(rec.evidence_id.starts_with("ev-"));
        assert_eq!(rec.status, EvidenceStatus::Passed);
        assert!(rec.stdout_hash.is_some());
        assert!(rec.stderr_hash.is_none());
        assert!(rec.is_success());
    }

    #[test]
    fn evidence_failed_status_when_exit_non_zero() {
        let rec =
            EvidenceRecord::from_command_output("task-1", "cargo test", 1, "", "boom", vec![]);
        assert_eq!(rec.status, EvidenceStatus::Failed);
        assert!(!rec.is_success());
        assert!(rec.stderr_hash.is_some());
    }

    #[test]
    fn state_digest_defaults_are_empty_lists() {
        let digest = StateDigest::new("objective", "verify");
        assert!(digest.blockers.is_empty());
        assert!(digest.recent_actions.is_empty());
        assert!(digest.next_action.is_none());
    }

    #[test]
    fn evidence_serializes_with_ts_utc_and_reads_legacy_timestamp_utc() {
        let record =
            EvidenceRecord::from_command_output("task-1", "cargo test", 0, "ok", "", Vec::new());
        let encoded = serde_json::to_value(&record).expect("serialize");
        assert!(encoded.get("ts_utc").is_some());
        assert!(encoded.get("timestamp_utc").is_none());

        let mut legacy = encoded;
        let ts = legacy.get("ts_utc").cloned().expect("ts_utc should exist");
        legacy
            .as_object_mut()
            .expect("object")
            .insert("timestamp_utc".to_string(), ts);
        legacy.as_object_mut().expect("object").remove("ts_utc");
        let decoded: EvidenceRecord = serde_json::from_value(legacy).expect("deserialize legacy");
        assert_eq!(decoded.task_id, "task-1");
    }
}
