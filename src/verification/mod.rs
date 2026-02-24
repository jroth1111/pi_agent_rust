//! Verification System for Pi Agent
//!
//! This module provides the verification infrastructure that ensures agent
//! changes are validated before task completion. It addresses:
//!
//! - Flaw 9: Clean-room verification with externally-defined test plans
//! - Flaw 10: Evidence-based completion gates (todos, tests, artifacts)
//! - Flaw 11: Cryptographic attestation tokens for verification results
//!
//! # Architecture
//!
//! The verification system consists of three layers:
//!
//! 1. **VerifyPlan**: Externally-defined test plans that the agent cannot modify.
//!    These specify what commands to run, timeouts, and expected outcomes.
//!
//! 2. **Evidence Collection**: Tracking artifacts that prove work was done correctly.
//!    This includes file contents, command outputs, test results, and diffs.
//!
//! 3. **Completion Gates**: Pre-conditions that must be satisfied before a task
//!    can be marked complete (open todos, passing tests, required evidence).
//!
//! 4. **Attestation Tokens**: Cryptographically-signed proof that verification
//!    passed, which can be verified by external systems.

mod evidence;
mod test_analyzer;
mod test_guard;
mod test_quality;
mod tokens;

pub use evidence::{
    CompletionError, CompletionGate, Evidence, EvidenceCollector, EvidenceKind, EvidenceStore,
};
pub use test_analyzer::{TestAnalyzer, TestQualityIssue};
pub use test_guard::{
    BuildState, BuildStateDiff, TestGate, TestGuard, TestGuardConfig, TestGuardResult, TestResult,
    VerificationContext, VerificationError, VerificationGate,
};
pub use test_quality::{TestQualityGate, calculate_quality_score};
pub use tokens::{
    AttestationManager, AttestationPayload, AttestationToken, TOKEN_VERSION, TokenError,
};

use crate::error::Result;
use crate::policy::ConstraintSet;
use crate::reliability::PlanRequirement;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Plan for verification - externally defined, agent cannot modify.
///
/// The verification plan specifies what commands to run and what outcomes
/// are considered successful. The agent cannot alter these parameters,
/// ensuring that verification criteria are determined by external policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyPlan {
    /// The command to run (e.g., "cargo test", "npm test")
    pub command: String,
    /// Timeout in seconds for the verification command
    pub timeout_secs: u64,
    /// Exit codes that indicate success (e.g., [0] for tests, [0, 1] for lint)
    pub expected_exit_codes: Vec<i32>,
    /// Required test perspectives to prevent tautological testing
    #[serde(default)]
    pub perspectives: Vec<TestPerspective>,
}

impl VerifyPlan {
    /// Create a new verification plan.
    pub fn new(
        command: impl Into<String>,
        timeout_secs: u64,
        expected_exit_codes: Vec<i32>,
    ) -> Self {
        Self {
            command: command.into(),
            timeout_secs,
            expected_exit_codes,
            perspectives: Vec::new(),
        }
    }

    /// Create a verification plan with test perspectives.
    pub fn with_perspectives(
        command: impl Into<String>,
        timeout_secs: u64,
        expected_exit_codes: Vec<i32>,
        perspectives: Vec<TestPerspective>,
    ) -> Self {
        Self {
            command: command.into(),
            timeout_secs,
            expected_exit_codes,
            perspectives,
        }
    }

    /// Create a simple test plan with default timeout and exit code 0.
    pub fn simple(command: impl Into<String>) -> Self {
        Self::new(command, 300, vec![0])
    }

    /// Get the timeout as a Duration.
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    /// Check if the given exit code indicates success.
    pub fn is_success_code(&self, code: i32) -> bool {
        self.expected_exit_codes.contains(&code)
    }

    /// Validate that all required perspectives are covered.
    pub fn validate_perspectives(&self, covered: &[TestPerspective]) -> bool {
        for required in &self.perspectives {
            if !covered.contains(required) {
                return false;
            }
        }
        true
    }
}

/// Required test perspectives to prevent tautological testing.
///
/// These perspectives ensure that verification covers different scenarios
/// rather than just the happy path that the agent expects to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestPerspective {
    /// Test the expected happy path where everything works correctly.
    HappyPath,
    /// Test negative cases: invalid inputs, error conditions, failures.
    Negative,
    /// Test boundary conditions: edge cases, limits, empty states.
    Boundary,
}

impl TestPerspective {
    /// Get a human-readable name for this perspective.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::HappyPath => "Happy Path",
            Self::Negative => "Negative Cases",
            Self::Boundary => "Boundary Conditions",
        }
    }

    /// Get a description of what this perspective tests.
    pub const fn description(&self) -> &'static str {
        match self {
            Self::HappyPath => "Tests the expected flow with valid inputs",
            Self::Negative => "Tests error handling with invalid inputs",
            Self::Boundary => "Tests edge cases and boundary values",
        }
    }
}

/// Validation that plan is sound before execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanValidation {
    /// Files the plan intends to touch
    pub touches: Vec<PathBuf>,
    /// Required test perspectives
    pub perspectives: Vec<TestPerspective>,
    /// Evidence references (file:line)
    pub evidence_refs: Vec<String>,
}

impl PlanValidation {
    /// Create a new plan validation
    pub const fn new(touches: Vec<PathBuf>, perspectives: Vec<TestPerspective>) -> Self {
        Self {
            touches,
            perspectives,
            evidence_refs: Vec::new(),
        }
    }

    /// Create a plan validation with evidence references
    pub const fn with_evidence(
        touches: Vec<PathBuf>,
        perspectives: Vec<TestPerspective>,
        evidence_refs: Vec<String>,
    ) -> Self {
        Self {
            touches,
            perspectives,
            evidence_refs,
        }
    }

    /// Validate plan meets requirements
    pub fn validate(
        &self,
        requirement: PlanRequirement,
        constraints: &ConstraintSet,
    ) -> Result<()> {
        match requirement {
            PlanRequirement::None => Ok(()),
            PlanRequirement::Optional => Ok(()),
            PlanRequirement::Required => {
                if self.touches.is_empty() {
                    return Err(crate::error::Error::Validation(
                        "Plan required: must specify files to touch".to_string(),
                    ));
                }
                if self.perspectives.is_empty() {
                    return Err(crate::error::Error::Validation(
                        "Plan required: must specify test perspectives".to_string(),
                    ));
                }

                // Check that touched files don't violate constraints
                for path in &self.touches {
                    let path_str = path.to_string_lossy().to_string();
                    constraints
                        .check_path(path, crate::policy::FileOperation::Change)
                        .map_err(|e| {
                            crate::error::Error::Validation(format!(
                                "Plan violates constraint on '{path_str}': {e}"
                            ))
                        })?;
                }

                Ok(())
            }
            PlanRequirement::RequiredWithEvidence => {
                // All of Required checks plus evidence
                if self.touches.is_empty() {
                    return Err(crate::error::Error::Validation(
                        "Plan required with evidence: must specify files to touch".to_string(),
                    ));
                }
                if self.perspectives.is_empty() {
                    return Err(crate::error::Error::Validation(
                        "Plan required with evidence: must specify test perspectives".to_string(),
                    ));
                }
                if self.evidence_refs.is_empty() {
                    return Err(crate::error::Error::Validation(
                        "Plan required with evidence: must provide evidence references".to_string(),
                    ));
                }

                // Validate evidence references format (file:line or similar)
                for ref_str in &self.evidence_refs {
                    if !ref_str.contains(':') || ref_str.len() < 3 {
                        return Err(crate::error::Error::Validation(format!(
                            "Invalid evidence reference '{ref_str}': expected format 'file:line'"
                        )));
                    }
                }

                // Check that touched files don't violate constraints
                for path in &self.touches {
                    let path_str = path.to_string_lossy().to_string();
                    constraints
                        .check_path(path, crate::policy::FileOperation::Change)
                        .map_err(|e| {
                            crate::error::Error::Validation(format!(
                                "Plan violates constraint on '{path_str}': {e}"
                            ))
                        })?;
                }

                Ok(())
            }
        }
    }

    /// Check if a specific perspective is included
    pub fn has_perspective(&self, perspective: TestPerspective) -> bool {
        self.perspectives.contains(&perspective)
    }

    /// Get the number of files to touch
    pub fn touch_count(&self) -> usize {
        self.touches.len()
    }
}

/// Result of verification with evidence.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    /// Whether the verification passed (exit code was expected)
    pub passed: bool,
    /// Exit code of the verification command
    pub exit_code: Option<i32>,
    /// Captured stdout from the verification command
    pub stdout: String,
    /// Captured stderr from the verification command
    pub stderr: String,
    /// IDs of evidence collected during verification
    pub evidence_collected: Vec<String>,
    /// Unix timestamp (ms) when verification completed
    pub timestamp: u64,
    /// Perspectives that were covered by this verification
    pub perspectives_covered: Vec<TestPerspective>,
}

impl VerificationOutcome {
    /// Create a new verification outcome.
    pub fn new(
        passed: bool,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        evidence_collected: Vec<String>,
        perspectives_covered: Vec<TestPerspective>,
    ) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Self {
            passed,
            exit_code,
            stdout,
            stderr,
            evidence_collected,
            timestamp,
            perspectives_covered,
        }
    }

    /// Create a failed verification outcome.
    pub fn failed(exit_code: Option<i32>, stderr: String) -> Self {
        Self::new(
            false,
            exit_code,
            String::new(),
            stderr,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Create a passed verification outcome.
    pub fn passed(stdout: String, evidence: Vec<String>) -> Self {
        Self::new(true, Some(0), stdout, String::new(), evidence, Vec::new())
    }

    /// Check if a specific perspective was covered.
    pub fn covers_perspective(&self, perspective: TestPerspective) -> bool {
        self.perspectives_covered.contains(&perspective)
    }

    /// Get a summary string for logging.
    pub fn summary(&self) -> String {
        if self.passed {
            format!(
                "Verification passed (exit code: {:?}, {} evidence items)",
                self.exit_code,
                self.evidence_collected.len()
            )
        } else {
            format!(
                "Verification failed (exit code: {:?}): {}",
                self.exit_code,
                self.stderr.chars().take(100).collect::<String>()
            )
        }
    }
}

/// Builder for creating verification plans.
#[derive(Debug, Default)]
pub struct VerifyPlanBuilder {
    command: Option<String>,
    timeout_secs: u64,
    expected_exit_codes: Vec<i32>,
    perspectives: Vec<TestPerspective>,
}

impl VerifyPlanBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            timeout_secs: 300,
            expected_exit_codes: vec![0],
            ..Default::default()
        }
    }

    /// Set the verification command.
    pub fn command(mut self, cmd: impl Into<String>) -> Self {
        self.command = Some(cmd.into());
        self
    }

    /// Set the timeout in seconds.
    pub const fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Add an expected exit code.
    pub fn expect_exit_code(mut self, code: i32) -> Self {
        if !self.expected_exit_codes.contains(&code) {
            self.expected_exit_codes.push(code);
        }
        self
    }

    /// Add a required test perspective.
    pub fn require_perspective(mut self, perspective: TestPerspective) -> Self {
        if !self.perspectives.contains(&perspective) {
            self.perspectives.push(perspective);
        }
        self
    }

    /// Build the verification plan.
    pub fn build(self) -> Option<VerifyPlan> {
        self.command.map(|cmd| VerifyPlan {
            command: cmd,
            timeout_secs: self.timeout_secs,
            expected_exit_codes: self.expected_exit_codes,
            perspectives: self.perspectives,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_plan_simple() {
        let plan = VerifyPlan::simple("cargo test");
        assert_eq!(plan.command, "cargo test");
        assert_eq!(plan.timeout_secs, 300);
        assert_eq!(plan.expected_exit_codes, vec![0]);
        assert!(plan.perspectives.is_empty());
    }

    #[test]
    fn verify_plan_success_code_check() {
        let plan = VerifyPlan::new("npm test", 60, vec![0]);
        assert!(plan.is_success_code(0));
        assert!(!plan.is_success_code(1));

        let lint_plan = VerifyPlan::new("eslint .", 60, vec![0, 1]);
        assert!(lint_plan.is_success_code(0));
        assert!(lint_plan.is_success_code(1));
        assert!(!lint_plan.is_success_code(2));
    }

    #[test]
    fn verify_plan_perspectives() {
        let plan = VerifyPlan::with_perspectives(
            "cargo test",
            300,
            vec![0],
            vec![TestPerspective::HappyPath, TestPerspective::Negative],
        );

        assert!(
            plan.validate_perspectives(&[TestPerspective::HappyPath, TestPerspective::Negative])
        );
        assert!(!plan.validate_perspectives(&[TestPerspective::HappyPath]));
    }

    #[test]
    fn test_perspective_names() {
        assert_eq!(TestPerspective::HappyPath.name(), "Happy Path");
        assert_eq!(TestPerspective::Negative.name(), "Negative Cases");
        assert_eq!(TestPerspective::Boundary.name(), "Boundary Conditions");
    }

    #[test]
    fn verification_outcome_summary() {
        let passed =
            VerificationOutcome::passed("All tests passed".to_string(), vec!["e1".to_string()]);
        assert!(passed.passed);
        assert!(passed.summary().contains("passed"));

        let failed = VerificationOutcome::failed(Some(1), "Test failed".to_string());
        assert!(!failed.passed);
        assert!(failed.summary().contains("failed"));
    }

    #[test]
    fn verify_plan_builder() {
        let plan = VerifyPlanBuilder::new()
            .command("cargo test")
            .timeout_secs(120)
            .expect_exit_code(0)
            .require_perspective(TestPerspective::HappyPath)
            .require_perspective(TestPerspective::Negative)
            .build()
            .expect("plan should build");

        assert_eq!(plan.command, "cargo test");
        assert_eq!(plan.timeout_secs, 120);
        assert_eq!(plan.expected_exit_codes, vec![0]);
        assert_eq!(plan.perspectives.len(), 2);
    }

    #[test]
    fn verify_plan_builder_without_command() {
        let plan = VerifyPlanBuilder::new().timeout_secs(60).build();
        assert!(plan.is_none());
    }

    // PlanValidation tests
    #[test]
    fn plan_validation_none_requires_nothing() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::new(vec![], vec![]);
        assert!(
            validation
                .validate(PlanRequirement::None, &constraints)
                .is_ok()
        );
    }

    #[test]
    fn plan_validation_optional_allows_empty() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::new(vec![], vec![]);
        assert!(
            validation
                .validate(PlanRequirement::Optional, &constraints)
                .is_ok()
        );
    }

    #[test]
    fn plan_validation_required_rejects_empty_touches() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::new(vec![], vec![TestPerspective::HappyPath]);
        assert!(
            validation
                .validate(PlanRequirement::Required, &constraints)
                .is_err()
        );
    }

    #[test]
    fn plan_validation_required_rejects_empty_perspectives() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::new(vec![PathBuf::from("src/main.rs")], vec![]);
        assert!(
            validation
                .validate(PlanRequirement::Required, &constraints)
                .is_err()
        );
    }

    #[test]
    fn plan_validation_required_accepts_valid() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::new(
            vec![PathBuf::from("src/main.rs")],
            vec![TestPerspective::HappyPath],
        );
        assert!(
            validation
                .validate(PlanRequirement::Required, &constraints)
                .is_ok()
        );
    }

    #[test]
    fn plan_validation_required_with_evidence_rejects_missing_evidence() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::new(
            vec![PathBuf::from("src/main.rs")],
            vec![TestPerspective::HappyPath],
        );
        assert!(
            validation
                .validate(PlanRequirement::RequiredWithEvidence, &constraints)
                .is_err()
        );
    }

    #[test]
    fn plan_validation_required_with_evidence_accepts_valid() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::with_evidence(
            vec![PathBuf::from("src/main.rs")],
            vec![TestPerspective::HappyPath],
            vec!["src/main.rs:42".to_string()],
        );
        assert!(
            validation
                .validate(PlanRequirement::RequiredWithEvidence, &constraints)
                .is_ok()
        );
    }

    #[test]
    fn plan_validation_rejects_invalid_evidence_format() {
        let constraints = ConstraintSet::new();
        let validation = PlanValidation::with_evidence(
            vec![PathBuf::from("src/main.rs")],
            vec![TestPerspective::HappyPath],
            vec!["invalid".to_string()],
        );
        assert!(
            validation
                .validate(PlanRequirement::RequiredWithEvidence, &constraints)
                .is_err()
        );
    }

    #[test]
    fn plan_validation_respects_no_touch_constraint() {
        let mut constraints = ConstraintSet::new();
        constraints.add_invariant(crate::policy::Invariant::no_touch("*.env"));

        let validation = PlanValidation::new(
            vec![PathBuf::from(".env")],
            vec![TestPerspective::HappyPath],
        );
        assert!(
            validation
                .validate(PlanRequirement::Required, &constraints)
                .is_err()
        );
    }

    #[test]
    fn plan_validation_has_perspective() {
        let validation = PlanValidation::new(
            vec![PathBuf::from("src/main.rs")],
            vec![TestPerspective::HappyPath, TestPerspective::Negative],
        );
        assert!(validation.has_perspective(TestPerspective::HappyPath));
        assert!(validation.has_perspective(TestPerspective::Negative));
        assert!(!validation.has_perspective(TestPerspective::Boundary));
    }

    #[test]
    fn plan_validation_touch_count() {
        let validation = PlanValidation::new(
            vec![
                PathBuf::from("src/main.rs"),
                PathBuf::from("src/lib.rs"),
                PathBuf::from("Cargo.toml"),
            ],
            vec![TestPerspective::HappyPath],
        );
        assert_eq!(validation.touch_count(), 3);
    }
}
