use crate::reliability::state::RuntimeState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub objective: String,
    pub constraints: TaskConstraintSet,
    pub verify: VerifyPlan,
    pub max_attempts: u8,
    pub input_snapshot: String,
    #[serde(default)]
    pub acceptance_ids: Vec<String>,
    #[serde(default)]
    pub planned_touches: Vec<String>,
}

impl TaskSpec {
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        if self.objective.trim().is_empty() {
            return Err(SpecValidationError("objective cannot be empty".to_string()));
        }
        if self.max_attempts == 0 {
            return Err(SpecValidationError(
                "max_attempts must be at least 1".to_string(),
            ));
        }
        if self.input_snapshot.trim().is_empty() {
            return Err(SpecValidationError(
                "input_snapshot cannot be empty".to_string(),
            ));
        }
        let VerifyPlan::Standard {
            command,
            timeout_sec,
            ..
        } = &self.verify;
        if command.trim().is_empty() {
            return Err(SpecValidationError(
                "verify command cannot be empty".to_string(),
            ));
        }
        if *timeout_sec == 0 {
            return Err(SpecValidationError(
                "verify timeout_sec must be at least 1".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskConstraintSet {
    pub invariants: Vec<String>,
    pub max_touched_files: Option<u16>,
    pub forbid_paths: Vec<String>,
    pub network_access: NetworkPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    #[default]
    Offline,
    Restricted(Vec<String>),
    Open,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyPlan {
    Standard {
        audit_diff: bool,
        command: String,
        timeout_sec: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRuntime {
    pub state: RuntimeState,
    pub attempt: u8,
    pub last_transition_at: DateTime<Utc>,
}

impl TaskRuntime {
    pub fn new() -> Self {
        Self {
            state: RuntimeState::Ready,
            attempt: 0,
            last_transition_at: Utc::now(),
        }
    }
}

impl Default for TaskRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,
    pub spec: TaskSpec,
    pub runtime: TaskRuntime,
    pub created_at: DateTime<Utc>,
}

impl TaskNode {
    pub fn new(id: String, spec: TaskSpec) -> Self {
        Self {
            id,
            spec,
            runtime: TaskRuntime::new(),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpecValidationError(String);

impl std::fmt::Display for SpecValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SpecValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_spec() -> TaskSpec {
        TaskSpec {
            objective: "Add retry guard".to_string(),
            constraints: TaskConstraintSet::default(),
            verify: VerifyPlan::Standard {
                audit_diff: true,
                command: "cargo test".to_string(),
                timeout_sec: 60,
            },
            max_attempts: 3,
            input_snapshot: "abc1234".to_string(),
            acceptance_ids: vec!["ac-1".to_string()],
            planned_touches: vec!["src/reliability/task.rs".to_string()],
        }
    }

    #[test]
    fn task_spec_validate_passes_on_valid_spec() {
        let spec = valid_spec();
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn task_spec_validate_rejects_invalid_fields() {
        let mut spec = valid_spec();
        spec.objective.clear();
        assert!(spec.validate().is_err());

        let mut spec = valid_spec();
        spec.max_attempts = 0;
        assert!(spec.validate().is_err());

        let mut spec = valid_spec();
        spec.input_snapshot.clear();
        assert!(spec.validate().is_err());

        let mut spec = valid_spec();
        spec.verify = VerifyPlan::Standard {
            audit_diff: false,
            command: String::new(),
            timeout_sec: 1,
        };
        assert!(spec.validate().is_err());

        let mut spec = valid_spec();
        spec.verify = VerifyPlan::Standard {
            audit_diff: false,
            command: "echo ok".to_string(),
            timeout_sec: 0,
        };
        assert!(spec.validate().is_err());
    }
}
