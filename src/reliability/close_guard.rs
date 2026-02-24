use crate::reliability::evidence::EvidenceRecord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseOutcomeKind {
    Success,
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosePayload {
    pub task_id: String,
    pub outcome: String,
    #[serde(default)]
    pub outcome_kind: Option<CloseOutcomeKind>,
    #[serde(default)]
    pub acceptance_ids: Vec<String>,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
    pub trace_parent: Option<String>,
}

impl ClosePayload {
    pub fn validate(&self) -> Result<(), CloseGuardError> {
        if self.task_id.trim().is_empty() {
            return Err(CloseGuardError::InvalidPayload(
                "task_id must not be empty".to_string(),
            ));
        }
        if self.outcome.trim().is_empty() {
            return Err(CloseGuardError::InvalidPayload(
                "outcome must not be empty".to_string(),
            ));
        }
        if self.acceptance_ids.is_empty() {
            return Err(CloseGuardError::MissingAcceptanceIds);
        }
        if self.evidence_ids.is_empty() {
            return Err(CloseGuardError::MissingEvidenceIds);
        }
        if self
            .trace_parent
            .as_ref()
            .is_none_or(|trace| trace.trim().is_empty())
        {
            return Err(CloseGuardError::MissingTraceParent);
        }
        if let Some(violation) = lint_outcome(self.outcome.as_str(), self.outcome_kind) {
            return Err(CloseGuardError::UnsafeOutcome(violation));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseResult {
    pub approved: bool,
    #[serde(default)]
    pub violations: Vec<String>,
}

impl CloseResult {
    pub const fn approved() -> Self {
        Self {
            approved: true,
            violations: Vec::new(),
        }
    }

    pub const fn rejected(violations: Vec<String>) -> Self {
        Self {
            approved: false,
            violations,
        }
    }

    pub fn ensure_approved(&self) -> Result<(), CloseGuardError> {
        if self.approved {
            Ok(())
        } else {
            Err(CloseGuardError::Rejected(self.violations.join("; ")))
        }
    }

    pub fn evaluate(
        payload: &ClosePayload,
        evidence: &[EvidenceRecord],
        require_success_evidence: bool,
    ) -> Result<Self, CloseGuardError> {
        payload.validate()?;

        let mut violations = Vec::new();
        for evidence_id in &payload.evidence_ids {
            if !evidence
                .iter()
                .any(|record| &record.evidence_id == evidence_id)
            {
                violations.push(format!("missing evidence record: {evidence_id}"));
            }
        }

        if require_success_evidence {
            let has_pass = evidence.iter().any(|record| {
                payload.evidence_ids.contains(&record.evidence_id) && record.is_success()
            });
            if !has_pass {
                violations.push("no passing evidence mapped to close payload".to_string());
            }
        }

        if violations.is_empty() {
            Ok(Self::approved())
        } else {
            Ok(Self::rejected(violations))
        }
    }
}

const UNSAFE_SUCCESS_KEYWORDS: &[&str] = &[
    "failed",
    "rejected",
    "wontfix",
    "won't fix",
    "canceled",
    "cancelled",
    "abandoned",
    "blocked",
    "error",
    "timeout",
    "aborted",
];

fn lint_outcome(outcome: &str, explicit_kind: Option<CloseOutcomeKind>) -> Option<String> {
    let normalized = outcome.trim().to_ascii_lowercase();
    let inferred_kind = if normalized.starts_with("failed: ") {
        CloseOutcomeKind::Failure
    } else {
        CloseOutcomeKind::Success
    };
    let outcome_kind = explicit_kind.unwrap_or(inferred_kind);

    if outcome_kind == CloseOutcomeKind::Failure && !normalized.starts_with("failed: ") {
        return Some("failure outcomes must start with `failed: `".to_string());
    }

    if outcome_kind == CloseOutcomeKind::Success {
        for keyword in UNSAFE_SUCCESS_KEYWORDS {
            if normalized.contains(keyword) {
                return Some(format!(
                    "success outcome contains unsafe keyword `{keyword}`"
                ));
            }
        }
    }

    None
}

#[derive(Debug, thiserror::Error)]
pub enum CloseGuardError {
    #[error("invalid close payload: {0}")]
    InvalidPayload(String),
    #[error("close payload missing acceptance IDs")]
    MissingAcceptanceIds,
    #[error("close payload missing evidence IDs")]
    MissingEvidenceIds,
    #[error("close payload missing trace parent reference")]
    MissingTraceParent,
    #[error("close payload outcome is unsafe: {0}")]
    UnsafeOutcome(String),
    #[error("close rejected: {0}")]
    Rejected(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::EvidenceRecord;

    #[test]
    fn close_payload_validation_requires_acceptance_and_evidence() {
        let payload = ClosePayload {
            task_id: "t1".to_string(),
            outcome: "implemented fix".to_string(),
            outcome_kind: Some(CloseOutcomeKind::Success),
            acceptance_ids: Vec::new(),
            evidence_ids: Vec::new(),
            trace_parent: None,
        };
        assert!(matches!(
            payload.validate().expect_err("must fail"),
            CloseGuardError::MissingAcceptanceIds
        ));
    }

    #[test]
    fn close_result_evaluate_requires_matching_and_passing_evidence() {
        let evidence =
            EvidenceRecord::from_command_output("t1", "cargo test", 1, "", "failure", Vec::new());
        let payload = ClosePayload {
            task_id: "t1".to_string(),
            outcome: "fixed".to_string(),
            outcome_kind: Some(CloseOutcomeKind::Success),
            acceptance_ids: vec!["ac-1".to_string()],
            evidence_ids: vec![evidence.evidence_id.clone()],
            trace_parent: Some("epic-1".to_string()),
        };

        let result = CloseResult::evaluate(&payload, &[evidence], true).expect("evaluate");
        assert!(!result.approved);
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn close_payload_rejects_success_reason_with_unsafe_keyword() {
        let payload = ClosePayload {
            task_id: "t1".to_string(),
            outcome: "Fixed error handling".to_string(),
            outcome_kind: Some(CloseOutcomeKind::Success),
            acceptance_ids: vec!["ac-1".to_string()],
            evidence_ids: vec!["ev-1".to_string()],
            trace_parent: Some("epic-1".to_string()),
        };

        let err = payload.validate().expect_err("must reject unsafe reason");
        assert!(matches!(err, CloseGuardError::UnsafeOutcome(_)));
    }

    #[test]
    fn close_payload_requires_failed_prefix_for_failure_outcomes() {
        let payload = ClosePayload {
            task_id: "t1".to_string(),
            outcome: "verification failed due to timeout".to_string(),
            outcome_kind: Some(CloseOutcomeKind::Failure),
            acceptance_ids: vec!["ac-1".to_string()],
            evidence_ids: vec!["ev-1".to_string()],
            trace_parent: Some("epic-1".to_string()),
        };

        let err = payload
            .validate()
            .expect_err("must reject unprefixed failure");
        assert!(matches!(err, CloseGuardError::UnsafeOutcome(_)));
    }

    #[test]
    fn close_payload_accepts_prefixed_failure_outcome() {
        let payload = ClosePayload {
            task_id: "t1".to_string(),
            outcome: "failed: verification command timed out".to_string(),
            outcome_kind: Some(CloseOutcomeKind::Failure),
            acceptance_ids: vec!["ac-1".to_string()],
            evidence_ids: vec!["ev-1".to_string()],
            trace_parent: Some("epic-1".to_string()),
        };

        assert!(payload.validate().is_ok());
    }

    #[test]
    fn close_payload_requires_trace_parent_reference() {
        let payload = ClosePayload {
            task_id: "t1".to_string(),
            outcome: "Implemented retry policy".to_string(),
            outcome_kind: Some(CloseOutcomeKind::Success),
            acceptance_ids: vec!["ac-1".to_string()],
            evidence_ids: vec!["ev-1".to_string()],
            trace_parent: None,
        };
        let err = payload.validate().expect_err("must require trace parent");
        assert!(matches!(err, CloseGuardError::MissingTraceParent));
    }
}
