//! Completion gates for task verification.
//!
//! Gates are verification checks that must pass before a task can be
//! marked as complete. This addresses Flaw 9 (Unverifiable Claims) by
//! requiring actual evidence rather than just LLM assertions.
//!
//! ## Usage
//!
//! ```ignore
//! let mut gates = CompletionGates::new();
//! gates.register(GateBuilder::new("tests-pass")
//!     .description("All unit tests must pass")
//!     .required(true)
//!     .checker(|| {
//!         // Run tests and return result
//!         Ok(GateResult::passed("42 tests passed"))
//!     })
//!     .build());
//!
//! // Before completing task:
//! gates.run_all()?; // Returns error if required gates fail
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

/// Result of running a completion gate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GateResult {
    /// Gate passed with optional message.
    Passed {
        /// Human-readable message about the result.
        message: String,
        /// Duration to run the gate (ms).
        duration_ms: u64,
    },
    /// Gate failed with reason.
    Failed {
        /// Human-readable reason for failure.
        reason: String,
        /// Duration to run the gate (ms).
        duration_ms: u64,
    },
    /// Gate was skipped (not required, or preconditions not met).
    Skipped {
        /// Reason for skipping.
        reason: String,
    },
}

impl GateResult {
    /// Creates a passed result.
    pub fn passed(message: impl Into<String>) -> Self {
        Self::Passed {
            message: message.into(),
            duration_ms: 0,
        }
    }

    /// Creates a passed result with duration.
    pub fn passed_with_duration(message: impl Into<String>, duration: Duration) -> Self {
        Self::Passed {
            message: message.into(),
            duration_ms: duration.as_millis() as u64,
        }
    }

    /// Creates a failed result.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self::Failed {
            reason: reason.into(),
            duration_ms: 0,
        }
    }

    /// Creates a failed result with duration.
    pub fn failed_with_duration(reason: impl Into<String>, duration: Duration) -> Self {
        Self::Failed {
            reason: reason.into(),
            duration_ms: duration.as_millis() as u64,
        }
    }

    /// Creates a skipped result.
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }

    /// Returns true if the gate passed.
    pub const fn is_pass(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }

    /// Returns true if the gate failed.
    pub const fn is_fail(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Returns true if the gate was skipped.
    pub const fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }

    /// Returns the message (pass message, fail reason, or skip reason).
    pub fn message(&self) -> &str {
        match self {
            Self::Passed { message, .. } => message,
            Self::Failed { reason, .. } => reason,
            Self::Skipped { reason } => reason,
        }
    }
}

/// Severity level for a gate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum GateSeverity {
    /// Gate must pass for task completion.
    #[default]
    Required,
    /// Gate is recommended but can be bypassed.
    Recommended,
    /// Gate is informational only.
    Informational,
}

/// A verification gate that checks a condition before task completion.
pub trait CompletionGate: Send + Sync {
    /// Unique name for this gate.
    fn name(&self) -> &str;

    /// Human-readable description.
    fn description(&self) -> &str;

    /// Severity level (determines if failure blocks completion).
    fn severity(&self) -> GateSeverity {
        GateSeverity::Required
    }

    /// Run the gate check.
    fn check(&self) -> crate::error::Result<GateResult>;
}

/// A simple closure-based gate implementation.
pub struct FnGate {
    name: String,
    description: String,
    severity: GateSeverity,
    checker: Box<dyn Fn() -> crate::error::Result<GateResult> + Send + Sync>,
}

impl FnGate {
    /// Creates a new function-based gate.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        severity: GateSeverity,
        checker: impl Fn() -> crate::error::Result<GateResult> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            severity,
            checker: Box::new(checker),
        }
    }
}

impl CompletionGate for FnGate {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn severity(&self) -> GateSeverity {
        self.severity
    }

    fn check(&self) -> crate::error::Result<GateResult> {
        (self.checker)()
    }
}

impl fmt::Debug for FnGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnGate")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("severity", &self.severity)
            .finish()
    }
}

/// A recorded result of running a gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateRecord {
    /// Name of the gate.
    pub name: String,
    /// Result of the check.
    pub result: GateResult,
    /// Severity of the gate.
    pub severity: GateSeverity,
    /// When the gate was run (unix timestamp ms).
    pub timestamp: u64,
}

/// Collection of completion gates with execution tracking.
pub struct CompletionGates {
    gates: HashMap<String, Box<dyn CompletionGate>>,
    /// Records of gate executions.
    records: Vec<GateRecord>,
}

impl Default for CompletionGates {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CompletionGates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionGates")
            .field("gate_count", &self.gates.len())
            .field("gate_names", &self.gates.keys().collect::<Vec<_>>())
            .field("records", &self.records)
            .finish()
    }
}

impl CompletionGates {
    /// Creates an empty gate collection.
    pub fn new() -> Self {
        Self {
            gates: HashMap::new(),
            records: Vec::new(),
        }
    }

    /// Registers a new gate.
    pub fn register<G: CompletionGate + 'static>(&mut self, gate: G) {
        self.gates.insert(gate.name().to_string(), Box::new(gate));
    }

    /// Registers a function-based gate.
    pub fn register_fn(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        severity: GateSeverity,
        checker: impl Fn() -> crate::error::Result<GateResult> + Send + Sync + 'static,
    ) {
        self.register(FnGate::new(name, description, severity, checker));
    }

    /// Returns the number of registered gates.
    pub fn count(&self) -> usize {
        self.gates.len()
    }

    /// Returns gate names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.gates.keys().map(std::string::String::as_str)
    }

    /// Runs a specific gate by name.
    pub fn run(&mut self, name: &str) -> crate::error::Result<Option<&GateRecord>> {
        if let Some(gate) = self.gates.get(name) {
            let result = gate.check()?;
            let severity = gate.severity();
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            let record = GateRecord {
                name: name.to_string(),
                result,
                severity,
                timestamp,
            };

            self.records.push(record);
            Ok(self.records.last())
        } else {
            Ok(None)
        }
    }

    /// Runs all registered gates.
    ///
    /// Returns `Ok` if all required gates passed.
    /// Returns `Err` if any required gate failed.
    pub fn run_all(&mut self) -> crate::error::Result<GateSummary> {
        let names: Vec<String> = self.gates.keys().cloned().collect();
        let mut summary = GateSummary::default();

        for name in names {
            if let Some(record) = self.run(&name)? {
                match record.severity {
                    GateSeverity::Required => {
                        if record.result.is_pass() {
                            summary.required_passed += 1;
                        } else if record.result.is_fail() {
                            summary.required_failed += 1;
                        }
                    }
                    GateSeverity::Recommended => {
                        if record.result.is_pass() {
                            summary.recommended_passed += 1;
                        } else if record.result.is_fail() {
                            summary.recommended_failed += 1;
                        }
                    }
                    GateSeverity::Informational => {
                        if record.result.is_pass() {
                            summary.informational_passed += 1;
                        } else if record.result.is_fail() {
                            summary.informational_failed += 1;
                        }
                    }
                }
            }
        }

        if summary.required_failed > 0 {
            Err(crate::error::Error::Validation(format!(
                "{} required gate(s) failed",
                summary.required_failed
            )))
        } else {
            Ok(summary)
        }
    }

    /// Returns all gate execution records.
    pub fn records(&self) -> &[GateRecord] {
        &self.records
    }

    /// Returns records for a specific gate.
    pub fn records_for(&self, name: &str) -> Vec<&GateRecord> {
        self.records.iter().filter(|r| r.name == name).collect()
    }

    /// Clears all records (keeps gates registered).
    pub fn clear_records(&mut self) {
        self.records.clear();
    }

    /// Returns true if all required gates have passed (based on records).
    pub fn all_required_passed(&self) -> bool {
        let required_names: Vec<&str> = self
            .gates
            .iter()
            .filter(|(_, g)| g.severity() == GateSeverity::Required)
            .map(|(n, _)| n.as_str())
            .collect();

        for name in required_names {
            let has_pass = self
                .records
                .iter()
                .any(|r| r.name == name && r.result.is_pass());
            if !has_pass {
                return false;
            }
        }
        true
    }
}

/// Summary of gate execution results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GateSummary {
    /// Number of required gates that passed.
    pub required_passed: usize,
    /// Number of required gates that failed.
    pub required_failed: usize,
    /// Number of recommended gates that passed.
    pub recommended_passed: usize,
    /// Number of recommended gates that failed.
    pub recommended_failed: usize,
    /// Number of informational gates that passed.
    pub informational_passed: usize,
    /// Number of informational gates that failed.
    pub informational_failed: usize,
}

impl GateSummary {
    /// Returns true if all required gates passed.
    pub const fn is_success(&self) -> bool {
        self.required_failed == 0
    }

    /// Returns total number of gates run.
    pub const fn total_run(&self) -> usize {
        self.required_passed
            + self.required_failed
            + self.recommended_passed
            + self.recommended_failed
            + self.informational_passed
            + self.informational_failed
    }

    /// Returns total number of passed gates.
    pub const fn total_passed(&self) -> usize {
        self.required_passed + self.recommended_passed + self.informational_passed
    }

    /// Returns total number of failed gates.
    pub const fn total_failed(&self) -> usize {
        self.required_failed + self.recommended_failed + self.informational_failed
    }
}

impl fmt::Display for GateSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Gates: {}/{} required, {}/{} recommended, {}/{} info",
            self.required_passed,
            self.required_passed + self.required_failed,
            self.recommended_passed,
            self.recommended_passed + self.recommended_failed,
            self.informational_passed,
            self.informational_passed + self.informational_failed,
        )
    }
}

/// Builder for creating completion gates.
pub struct GateBuilder {
    name: String,
    description: String,
    severity: GateSeverity,
}

impl GateBuilder {
    /// Creates a new gate builder.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            severity: GateSeverity::Required,
        }
    }

    /// Sets the description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the severity to required (default).
    pub const fn required(mut self) -> Self {
        self.severity = GateSeverity::Required;
        self
    }

    /// Sets the severity to recommended.
    pub const fn recommended(mut self) -> Self {
        self.severity = GateSeverity::Recommended;
        self
    }

    /// Sets the severity to informational.
    pub const fn informational(mut self) -> Self {
        self.severity = GateSeverity::Informational;
        self
    }

    /// Builds a function-based gate.
    pub fn build_fn(
        self,
        checker: impl Fn() -> crate::error::Result<GateResult> + Send + Sync + 'static,
    ) -> FnGate {
        FnGate::new(self.name, self.description, self.severity, checker)
    }
}

/// A gate that requires non-empty evidence for success claims.
///
/// This gate checks that at least one piece of evidence was collected
/// before allowing task completion. This helps ensure that completion
/// claims are backed by actual verification.
///
/// # Example
///
/// ```ignore
/// let gate = EvidenceGate::new();
/// let result = gate.check(&[]);  // Returns failure
///
/// let evidence = vec![
///     Evidence::TestOutput { passed: 5, failed: 0, output: "OK".to_string() },
/// ];
/// let result = gate.check(&evidence);  // Returns success
/// ```
pub struct EvidenceGate {
    /// Severity level for the gate.
    severity: GateSeverity,
    /// Minimum number of evidence items required (default: 1).
    min_evidence: usize,
}

impl EvidenceGate {
    /// Creates a new evidence gate with recommended severity.
    pub const fn new() -> Self {
        Self {
            severity: GateSeverity::Recommended,
            min_evidence: 1,
        }
    }

    /// Creates a gate with required severity.
    pub const fn required() -> Self {
        Self {
            severity: GateSeverity::Required,
            min_evidence: 1,
        }
    }

    /// Creates a gate with informational severity only.
    pub const fn informational() -> Self {
        Self {
            severity: GateSeverity::Informational,
            min_evidence: 1,
        }
    }

    /// Sets the minimum evidence count required.
    pub fn with_min_evidence(mut self, min: usize) -> Self {
        self.min_evidence = min.max(1);
        self
    }

    /// Sets the severity level.
    pub const fn with_severity(mut self, severity: GateSeverity) -> Self {
        self.severity = severity;
        self
    }

    /// Checks if the evidence meets the gate's requirements.
    pub fn check(&self, evidence: &[crate::state::Evidence]) -> GateResult {
        if evidence.len() >= self.min_evidence {
            GateResult::passed(format!(
                "Evidence gate passed: {} item(s) collected",
                evidence.len()
            ))
        } else {
            GateResult::failed(format!(
                "Evidence gate failed: {} evidence required, only {} collected",
                self.min_evidence,
                evidence.len()
            ))
        }
    }
}

impl Default for EvidenceGate {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionGate for EvidenceGate {
    fn name(&self) -> &'static str {
        "evidence_gate"
    }

    fn description(&self) -> &'static str {
        "Requires non-empty evidence for task completion"
    }

    fn severity(&self) -> GateSeverity {
        self.severity
    }

    fn check(&self) -> crate::error::Result<GateResult> {
        // This method is required by the trait, but EvidenceGate checks
        // evidence at runtime. The actual check is done via `check_evidence`.
        Ok(GateResult::skipped(
            "Use check_evidence() to verify evidence requirements",
        ))
    }
}

impl EvidenceGate {
    /// Checks evidence against the gate requirements.
    ///
    /// This is the main method to use when verifying evidence requirements.
    pub fn check_evidence(&self, evidence: &[crate::state::Evidence]) -> GateResult {
        if evidence.len() >= self.min_evidence {
            GateResult::passed(format!(
                "Evidence gate passed: {} item(s) collected",
                evidence.len()
            ))
        } else {
            GateResult::failed(format!(
                "Evidence gate failed: {} evidence required, only {} collected",
                self.min_evidence,
                evidence.len()
            ))
        }
    }

    /// Returns a summary of evidence kinds present.
    pub fn evidence_summary(&self, evidence: &[crate::state::Evidence]) -> String {
        let kinds: std::collections::HashSet<&str> = evidence
            .iter()
            .map(super::events::Evidence::kind_name)
            .collect();
        format!(
            "{} evidence item(s): {}",
            evidence.len(),
            kinds.into_iter().collect::<Vec<_>>().join(", ")
        )
    }
}

impl fmt::Debug for EvidenceGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvidenceGate")
            .field("severity", &self.severity)
            .field("min_evidence", &self.min_evidence)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_result_helpers() {
        let passed = GateResult::passed("All good");
        assert!(passed.is_pass());
        assert!(!passed.is_fail());
        assert_eq!(passed.message(), "All good");

        let failed = GateResult::failed("Something went wrong");
        assert!(failed.is_fail());
        assert!(!failed.is_pass());
        assert_eq!(failed.message(), "Something went wrong");

        let skipped = GateResult::skipped("Not applicable");
        assert!(skipped.is_skipped());
    }

    #[test]
    fn fn_gate_creation() {
        let gate = FnGate::new("test-gate", "A test gate", GateSeverity::Required, || {
            Ok(GateResult::passed("OK"))
        });

        assert_eq!(gate.name(), "test-gate");
        assert_eq!(gate.description(), "A test gate");
        assert_eq!(gate.severity(), GateSeverity::Required);
    }

    #[test]
    fn completion_gates_register_and_run() {
        let mut gates = CompletionGates::new();

        gates.register_fn(
            "always-pass",
            "Always passes",
            GateSeverity::Required,
            || Ok(GateResult::passed("OK")),
        );

        assert_eq!(gates.count(), 1);

        let record = gates.run("always-pass").unwrap().unwrap();
        assert!(record.result.is_pass());
    }

    #[test]
    fn completion_gates_run_all() {
        let mut gates = CompletionGates::new();

        gates.register_fn("pass-gate", "Passes", GateSeverity::Required, || {
            Ok(GateResult::passed("OK"))
        });
        gates.register_fn("fail-gate", "Fails", GateSeverity::Recommended, || {
            Ok(GateResult::failed("Not OK"))
        });

        let summary = gates.run_all().unwrap();

        assert_eq!(summary.required_passed, 1);
        assert_eq!(summary.recommended_failed, 1);
        assert!(summary.is_success());
    }

    #[test]
    fn completion_gates_required_failure() {
        let mut gates = CompletionGates::new();

        gates.register_fn(
            "fail-required",
            "Fails required",
            GateSeverity::Required,
            || Ok(GateResult::failed("Critical failure")),
        );

        let result = gates.run_all();
        assert!(result.is_err());
    }

    #[test]
    fn gate_builder() {
        let gate = GateBuilder::new("builder-test")
            .description("Built with builder")
            .recommended()
            .build_fn(|| Ok(GateResult::passed("Built")));

        assert_eq!(gate.name(), "builder-test");
        assert_eq!(gate.severity(), GateSeverity::Recommended);
    }

    #[test]
    fn gate_summary_display() {
        let summary = GateSummary {
            required_passed: 2,
            required_failed: 0,
            recommended_passed: 1,
            recommended_failed: 1,
            informational_passed: 0,
            informational_failed: 0,
        };

        let display = format!("{}", summary);
        assert!(display.contains("2/2 required"));
        assert!(display.contains("1/2 recommended"));
    }

    #[test]
    fn all_required_passed_check() {
        let mut gates = CompletionGates::new();

        gates.register_fn("req1", "Required 1", GateSeverity::Required, || {
            Ok(GateResult::passed("OK"))
        });
        gates.register_fn("req2", "Required 2", GateSeverity::Required, || {
            Ok(GateResult::passed("OK"))
        });
        gates.register_fn("rec1", "Recommended 1", GateSeverity::Recommended, || {
            Ok(GateResult::failed("Not OK"))
        });

        // Run only req1
        gates.run("req1").unwrap();
        assert!(!gates.all_required_passed()); // req2 not run yet

        // Run req2
        gates.run("req2").unwrap();
        assert!(gates.all_required_passed()); // Both required passed

        // Recommended failing doesn't affect the check
        gates.run("rec1").unwrap();
        assert!(gates.all_required_passed());
    }

    // EvidenceGate tests

    #[test]
    fn evidence_gate_new() {
        let gate = EvidenceGate::new();
        assert_eq!(gate.severity(), GateSeverity::Recommended);
        assert_eq!(gate.min_evidence, 1);
    }

    #[test]
    fn evidence_gate_required() {
        let gate = EvidenceGate::required();
        assert_eq!(gate.severity(), GateSeverity::Required);
    }

    #[test]
    fn evidence_gate_with_min_evidence() {
        let gate = EvidenceGate::new().with_min_evidence(3);
        assert_eq!(gate.min_evidence, 3);
    }

    #[test]
    fn evidence_gate_check_empty() {
        let gate = EvidenceGate::new();
        let result = gate.check_evidence(&[]);
        assert!(result.is_fail());
        assert!(result.message().contains("1 evidence required"));
    }

    #[test]
    fn evidence_gate_check_with_evidence() {
        use crate::state::Evidence;

        let gate = EvidenceGate::new();
        let evidence = vec![Evidence::TestOutput {
            passed: 5,
            failed: 0,
            output: "OK".to_string(),
        }];

        let result = gate.check_evidence(&evidence);
        assert!(result.is_pass());
        assert!(result.message().contains("1 item(s) collected"));
    }

    #[test]
    fn evidence_gate_check_min_evidence() {
        use crate::state::Evidence;

        let gate = EvidenceGate::new().with_min_evidence(2);
        let evidence = vec![Evidence::TestOutput {
            passed: 5,
            failed: 0,
            output: "OK".to_string(),
        }];

        let result = gate.check_evidence(&evidence);
        assert!(result.is_fail());
        assert!(result.message().contains("2 evidence required"));
    }

    #[test]
    fn evidence_gate_summary() {
        use crate::state::Evidence;

        let gate = EvidenceGate::new();
        let evidence = vec![
            Evidence::TestOutput {
                passed: 5,
                failed: 0,
                output: "OK".to_string(),
            },
            Evidence::GitDiff {
                files_changed: 2,
                diff: "diff".to_string(),
            },
        ];

        let summary = gate.evidence_summary(&evidence);
        assert!(summary.contains("2 evidence item(s)"));
        assert!(summary.contains("test_output"));
        assert!(summary.contains("git_diff"));
    }

    #[test]
    fn evidence_gate_severity_levels() {
        let recommended = EvidenceGate::new();
        let required = EvidenceGate::required();
        let informational = EvidenceGate::informational();

        assert_eq!(recommended.severity(), GateSeverity::Recommended);
        assert_eq!(required.severity(), GateSeverity::Required);
        assert_eq!(informational.severity(), GateSeverity::Informational);
    }
}
