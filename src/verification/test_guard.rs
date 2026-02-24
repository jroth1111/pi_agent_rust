//! Test Guard Infrastructure
//!
//! This module provides the test execution guard that enforces verification
//! before task completion. It ensures LLM claims are verified with actual
//! execution and proper evidence collection.

use crate::error::{Error, Result};
use crate::tools::run_bash_command;
use crate::verification::{
    VerificationOutcome,
    evidence::{Evidence, EvidenceKind, EvidenceStore},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Configuration for test execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGuardConfig {
    /// Whether tests are required for completion
    pub required: bool,
    /// Test command to run (e.g., "cargo test")
    pub test_command: String,
    /// Timeout in seconds for test execution
    pub timeout_secs: u64,
    /// Path where test evidence should be stored
    pub evidence_path: PathBuf,
    /// Whether to allow override (skip tests with flag)
    pub allow_override: bool,
    /// Whether to capture build state before/after
    pub capture_build_state: bool,
}

impl Default for TestGuardConfig {
    fn default() -> Self {
        Self {
            required: true,
            test_command: "cargo test".to_string(),
            timeout_secs: 300,
            evidence_path: PathBuf::from(".artifacts/test_evidence"),
            allow_override: false,
            capture_build_state: true,
        }
    }
}

impl TestGuardConfig {
    /// Create a test guard config with custom command.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            test_command: command.into(),
            ..Default::default()
        }
    }

    /// Set the timeout for test execution.
    pub const fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Set whether tests are required.
    pub const fn with_required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Set whether override is allowed.
    pub const fn with_allow_override(mut self, allow: bool) -> Self {
        self.allow_override = allow;
        self
    }

    /// Set the evidence path.
    pub fn with_evidence_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.evidence_path = path.into();
        self
    }

    /// Enable/disable build state capture.
    pub const fn with_build_state(mut self, capture: bool) -> Self {
        self.capture_build_state = capture;
        self
    }
}

/// Result of running tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Whether all tests passed
    pub passed: bool,
    /// Exit code from test command
    pub exit_code: i32,
    /// Number of tests run
    pub test_count: usize,
    /// Number of tests passed
    pub passed_count: usize,
    /// Number of tests failed
    pub failed_count: usize,
    /// Captured stdout from test execution
    pub stdout: String,
    /// Captured stderr from test execution
    pub stderr: String,
    /// Duration of test execution
    pub duration_ms: u64,
}

impl TestResult {
    /// Create a successful test result.
    pub const fn success(stdout: String, duration_ms: u64) -> Self {
        Self {
            passed: true,
            exit_code: 0,
            test_count: 0,
            passed_count: 0,
            failed_count: 0,
            stdout,
            stderr: String::new(),
            duration_ms,
        }
    }

    /// Create a failed test result.
    pub const fn failure(exit_code: i32, stderr: String, duration_ms: u64) -> Self {
        Self {
            passed: false,
            exit_code,
            test_count: 0,
            passed_count: 0,
            failed_count: 0,
            stdout: String::new(),
            stderr,
            duration_ms,
        }
    }

    /// Check if tests passed.
    pub const fn is_success(&self) -> bool {
        self.passed
    }
}

/// Build state snapshot for comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildState {
    /// Git commit hash (if available)
    pub commit: Option<String>,
    /// Timestamp when snapshot was taken
    pub timestamp: u64,
    /// List of modified files (from git status)
    pub modified_files: Vec<String>,
    /// Cargo check status (if applicable)
    pub check_status: Option<String>,
    /// Build errors (if any)
    pub build_errors: Vec<String>,
}

impl BuildState {
    /// Capture the current build state.
    pub async fn capture(cwd: &Path) -> Result<Self> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Try to get git commit
        let commit =
            match run_bash_command(cwd, None, None, "git rev-parse HEAD", Some(5), None).await {
                Ok(result) if result.exit_code == 0 => Some(result.output.trim().to_string()),
                _ => None,
            };

        // Get modified files
        let modified_files = match run_bash_command(
            cwd,
            None,
            None,
            "git status --porcelain",
            Some(5),
            None,
        )
        .await
        {
            Ok(result) if result.exit_code == 0 => result
                .output
                .lines()
                .filter(|line| !line.is_empty())
                .map(std::string::ToString::to_string)
                .collect(),
            _ => Vec::new(),
        };

        // Run cargo check to see build state
        let (check_status, build_errors) =
            match run_bash_command(cwd, None, None, "cargo check 2>&1", Some(60), None).await {
                Ok(result) => {
                    if result.exit_code == 0 {
                        (Some("ok".to_string()), Vec::new())
                    } else {
                        let errors = parse_cargo_errors(&result.output);
                        (Some("failed".to_string()), errors)
                    }
                }
                Err(_) => (None, Vec::new()),
            };

        Ok(Self {
            commit,
            timestamp,
            modified_files,
            check_status,
            build_errors,
        })
    }

    /// Compare two build states and return a diff summary.
    pub fn compare(&self, other: &Self) -> BuildStateDiff {
        BuildStateDiff {
            duration_secs: other.timestamp.saturating_sub(self.timestamp),
            files_changed: other.modified_files.len(),
            new_files: other
                .modified_files
                .iter()
                .filter(|f| !self.modified_files.contains(f))
                .cloned()
                .collect(),
            build_status_changed: self.check_status != other.check_status,
            new_errors: other
                .build_errors
                .iter()
                .filter(|e| !self.build_errors.contains(e))
                .cloned()
                .collect(),
        }
    }
}

/// Diff between two build states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStateDiff {
    /// Time elapsed between snapshots (seconds)
    pub duration_secs: u64,
    /// Number of files changed
    pub files_changed: usize,
    /// Files that appear in the new state but not the old
    pub new_files: Vec<String>,
    /// Whether the build status changed
    pub build_status_changed: bool,
    /// New build errors that appeared
    pub new_errors: Vec<String>,
}

/// Parse cargo errors from output.
fn parse_cargo_errors(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.contains("error") || line.contains("Error"))
        .map(|line| line.trim().to_string())
        .collect()
}

/// Test guard that enforces test execution before completion.
#[derive(Debug, Clone)]
pub struct TestGuard {
    config: TestGuardConfig,
    evidence_store: EvidenceStore,
}

impl TestGuard {
    /// Create a new test guard with default config.
    pub fn new(config: TestGuardConfig) -> Self {
        Self {
            config,
            evidence_store: EvidenceStore::new(),
        }
    }

    /// Create a test guard with default configuration.
    pub fn default() -> Self {
        Self::new(TestGuardConfig::default())
    }

    /// Get the evidence store for this guard.
    pub const fn evidence_store(&self) -> &EvidenceStore {
        &self.evidence_store
    }

    /// Verify that tests pass, collecting evidence.
    pub async fn verify(&self, cwd: &Path, override_flag: bool) -> Result<TestGuardResult> {
        // Check if override is allowed and requested
        if override_flag && self.config.allow_override {
            return Ok(TestGuardResult::Overridden);
        }

        // If tests aren't required, skip with warning
        if !self.config.required {
            return Ok(TestGuardResult::Skipped {
                reason: "tests not required by configuration".to_string(),
            });
        }

        // Capture build state before running tests
        let before_state = if self.config.capture_build_state {
            Some(BuildState::capture(cwd).await?)
        } else {
            None
        };

        // Run the test command
        let test_result =
            run_tests(cwd, &self.config.test_command, self.config.timeout_secs).await?;

        // Capture build state after running tests
        let after_state = if self.config.capture_build_state {
            Some(BuildState::capture(cwd).await?)
        } else {
            None
        };

        // Calculate build state diff
        let state_diff = if let (Some(before), Some(after)) = (&before_state, &after_state) {
            Some(before.compare(after))
        } else {
            None
        };

        // Collect evidence
        let evidence_ids = self
            .collect_test_evidence(&test_result, &before_state, &after_state)
            .await?;

        // Return result based on test outcome
        if test_result.is_success() {
            Ok(TestGuardResult::Passed {
                test_result,
                evidence_ids,
                state_diff,
            })
        } else {
            Ok(TestGuardResult::Failed {
                test_result,
                evidence_ids,
                state_diff,
            })
        }
    }

    /// Collect evidence from test execution.
    async fn collect_test_evidence(
        &self,
        test_result: &TestResult,
        before_state: &Option<BuildState>,
        after_state: &Option<BuildState>,
    ) -> Result<Vec<String>> {
        let mut evidence_ids = Vec::new();

        // Add test command output as evidence
        let output_evidence = Evidence::from_command(
            &self.config.test_command,
            format!(
                "stdout:\n{}\nstderr:\n{}",
                test_result.stdout, test_result.stderr
            ),
        );
        let output_id = output_evidence.id.clone();
        self.evidence_store.add(output_evidence)?;
        evidence_ids.push(output_id);

        // Add before state as evidence
        if let Some(before) = before_state {
            let before_json =
                serde_json::to_string(before).map_err(|e| Error::Json(Box::new(e)))?;
            let before_evidence = Evidence::new(
                format!("build-state-before-{}", uuid::Uuid::new_v4()),
                EvidenceKind::FileContent {
                    path: "before".to_string(),
                },
                before_json,
                "build-state",
            );
            let before_id = before_evidence.id.clone();
            self.evidence_store.add(before_evidence)?;
            evidence_ids.push(before_id);
        }

        // Add after state as evidence
        if let Some(after) = after_state {
            let after_json = serde_json::to_string(after).map_err(|e| Error::Json(Box::new(e)))?;
            let after_evidence = Evidence::new(
                format!("build-state-after-{}", uuid::Uuid::new_v4()),
                EvidenceKind::FileContent {
                    path: "after".to_string(),
                },
                after_json,
                "build-state",
            );
            let after_id = after_evidence.id.clone();
            self.evidence_store.add(after_evidence)?;
            evidence_ids.push(after_id);
        }

        Ok(evidence_ids)
    }

    /// Export all collected evidence to a file.
    pub fn export_evidence(&self, path: &Path) -> Result<()> {
        let json = self.evidence_store.to_json()?;
        std::fs::write(path, serde_json::to_string_pretty(&json)?)
            .map_err(|e| Error::Io(Box::new(e)))?;
        Ok(())
    }
}

/// Result of test guard verification.
#[derive(Debug, Clone)]
pub enum TestGuardResult {
    /// Tests passed successfully
    Passed {
        test_result: TestResult,
        evidence_ids: Vec<String>,
        state_diff: Option<BuildStateDiff>,
    },
    /// Tests failed
    Failed {
        test_result: TestResult,
        evidence_ids: Vec<String>,
        state_diff: Option<BuildStateDiff>,
    },
    /// Tests were skipped (not required)
    Skipped { reason: String },
    /// Tests were overridden
    Overridden,
}

impl TestGuardResult {
    /// Check if the guard allows completion.
    pub const fn allows_completion(&self) -> bool {
        match self {
            Self::Passed { .. } => true,
            Self::Skipped { .. } => true, // Skipped means not required, so allow
            Self::Overridden => true,     // Override explicitly allows
            Self::Failed { .. } => false, // Failed tests block completion
        }
    }

    /// Get a summary message for logging.
    pub fn summary(&self) -> String {
        match self {
            Self::Passed { test_result, .. } => {
                format!("Tests passed in {}ms", test_result.duration_ms)
            }
            Self::Failed { test_result, .. } => {
                format!(
                    "Tests failed (exit code {}): {}",
                    test_result.exit_code,
                    test_result.stderr.chars().take(100).collect::<String>()
                )
            }
            Self::Skipped { reason } => format!("Tests skipped: {reason}"),
            Self::Overridden => "Tests overridden by flag".to_string(),
        }
    }
}

/// Run tests with the given command.
async fn run_tests(cwd: &Path, command: &str, timeout_secs: u64) -> Result<TestResult> {
    let start = std::time::Instant::now();

    let result = run_bash_command(cwd, None, None, command, Some(timeout_secs), None)
        .await
        .map_err(|e| Error::tool("test_guard", format!("Failed to run test command: {e}")))?;

    let duration_ms = start.elapsed().as_millis() as u64;

    if result.exit_code == 0 {
        Ok(TestResult::success(result.output, duration_ms))
    } else {
        Ok(TestResult::failure(
            result.exit_code,
            result.output,
            duration_ms,
        ))
    }
}

/// Convert test guard result to verification outcome.
impl From<TestGuardResult> for VerificationOutcome {
    fn from(result: TestGuardResult) -> Self {
        match result {
            TestGuardResult::Passed {
                test_result,
                evidence_ids,
                ..
            } => Self::new(
                true,
                Some(test_result.exit_code),
                test_result.stdout,
                test_result.stderr,
                evidence_ids,
                Vec::new(),
            ),
            TestGuardResult::Failed {
                test_result,
                evidence_ids,
                ..
            } => Self::new(
                false,
                Some(test_result.exit_code),
                test_result.stdout,
                test_result.stderr,
                evidence_ids,
                Vec::new(),
            ),
            TestGuardResult::Skipped { .. } => Self::new(
                true,
                Some(0),
                "Skipped".to_string(),
                String::new(),
                Vec::new(),
                Vec::new(),
            ),
            TestGuardResult::Overridden => Self::new(
                true,
                Some(0),
                "Overridden".to_string(),
                String::new(),
                Vec::new(),
                Vec::new(),
            ),
        }
    }
}

/// Verification gate trait for extensible verification strategies.
pub trait VerificationGate: Send + Sync {
    /// Verify that the gate conditions are met.
    fn verify(&self, context: &VerificationContext) -> std::result::Result<(), VerificationError>;

    /// Get a human-readable description of this gate.
    fn description(&self) -> &str;
}

/// Context provided to verification gates.
#[derive(Debug, Clone)]
pub struct VerificationContext {
    /// Working directory for verification
    pub cwd: PathBuf,
    /// Evidence collected so far
    pub evidence: Vec<Evidence>,
    /// Whether override is requested
    pub override_requested: bool,
}

/// Errors that can occur during verification.
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("Tests failed: {0}")]
    TestsFailed(String),

    #[error("Evidence missing: {0}")]
    EvidenceMissing(String),

    #[error("Verification command error: {0}")]
    CommandError(String),

    #[error("Gate '{0}' rejected: {1}")]
    GateRejected(String, String),
}

/// Test gate implementation of VerificationGate.
pub struct TestGate {
    required: bool,
    command: String,
    timeout: Duration,
}

impl TestGate {
    /// Create a new test gate.
    pub fn new(command: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            required: true,
            command: command.into(),
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// Set whether tests are required.
    pub const fn with_required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }
}

impl VerificationGate for TestGate {
    fn verify(&self, context: &VerificationContext) -> std::result::Result<(), VerificationError> {
        if !self.required && !context.override_requested {
            return Ok(());
        }

        // Check for existing test evidence
        let has_test_evidence = context
            .evidence
            .iter()
            .any(|e| matches!(e.kind, EvidenceKind::TestResult { .. }));

        if !has_test_evidence {
            return Err(VerificationError::EvidenceMissing(
                "No test evidence found".to_string(),
            ));
        }

        Ok(())
    }

    fn description(&self) -> &'static str {
        "Test execution gate"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guard_config_default() {
        let config = TestGuardConfig::default();
        assert!(config.required);
        assert_eq!(config.test_command, "cargo test");
        assert_eq!(config.timeout_secs, 300);
    }

    #[test]
    fn test_guard_config_builder() {
        let config = TestGuardConfig::new("npm test")
            .with_timeout(120)
            .with_required(false)
            .with_allow_override(true);

        assert!(!config.required);
        assert_eq!(config.test_command, "npm test");
        assert_eq!(config.timeout_secs, 120);
        assert!(config.allow_override);
    }

    #[test]
    fn test_result_success() {
        let result = TestResult::success("All tests passed".to_string(), 1000);
        assert!(result.is_success());
        assert!(result.passed);
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn test_result_failure() {
        let result = TestResult::failure(1, "Test failed".to_string(), 500);
        assert!(!result.is_success());
        assert!(!result.passed);
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn test_guard_result_allows_completion() {
        let passed = TestGuardResult::Passed {
            test_result: TestResult::success(String::new(), 100),
            evidence_ids: vec!["e1".to_string()],
            state_diff: None,
        };
        assert!(passed.allows_completion());

        let failed = TestGuardResult::Failed {
            test_result: TestResult::failure(1, String::new(), 100),
            evidence_ids: vec!["e1".to_string()],
            state_diff: None,
        };
        assert!(!failed.allows_completion());

        let skipped = TestGuardResult::Skipped {
            reason: "not required".to_string(),
        };
        assert!(skipped.allows_completion());

        assert!(TestGuardResult::Overridden.allows_completion());
    }

    #[test]
    fn test_gate_verify_requires_test_evidence() {
        let gate = TestGate::new("cargo test", 300);

        let context_with_evidence = VerificationContext {
            cwd: PathBuf::from("."),
            evidence: vec![Evidence::from_test("test_foo", "passed")],
            override_requested: false,
        };
        assert!(gate.verify(&context_with_evidence).is_ok());

        let context_without_evidence = VerificationContext {
            cwd: PathBuf::from("."),
            evidence: Vec::new(),
            override_requested: false,
        };
        assert!(gate.verify(&context_without_evidence).is_err());
    }

    #[test]
    fn test_gate_non_required_skips_verification() {
        let gate = TestGate::new("cargo test", 300).with_required(false);

        let context = VerificationContext {
            cwd: PathBuf::from("."),
            evidence: Vec::new(),
            override_requested: false,
        };
        assert!(gate.verify(&context).is_ok());
    }

    #[test]
    fn parse_cargo_errors_from_output() {
        let output = r#"
error[E0308]: mismatched types
  --> src/main.rs:5:10
   |
5 |     let x: i32 = "hello";
   |            ---   ^^^^^^^ expected `i32`, found `&str`
   |            |
   |            expected: `i32`
   |            found: `&str`

error: aborting due to previous error
"#;
        let errors = parse_cargo_errors(output);
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|e| e.contains("error[E0308]")));
    }

    #[test]
    fn build_state_compare() {
        let before = BuildState {
            commit: Some("abc123".to_string()),
            timestamp: 1000,
            modified_files: vec!["src/main.rs".to_string()],
            check_status: Some("ok".to_string()),
            build_errors: Vec::new(),
        };

        let after = BuildState {
            commit: Some("abc123".to_string()),
            timestamp: 1010,
            modified_files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            check_status: Some("ok".to_string()),
            build_errors: Vec::new(),
        };

        let diff = before.compare(&after);
        assert_eq!(diff.duration_secs, 10);
        assert_eq!(diff.files_changed, 2);
        assert!(diff.new_files.contains(&"src/lib.rs".to_string()));
    }
}
