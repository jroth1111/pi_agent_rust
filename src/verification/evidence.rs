//! Evidence collection and completion gates for verification.
//!
//! This module provides the evidence tracking system that proves work was
//! done correctly and the completion gates that enforce pre-conditions
//! before task completion is allowed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// Evidence that a task was completed correctly.
///
/// Evidence represents proof that specific work was performed. It can be
/// file contents, command outputs, test results, or diffs. Each piece of
/// evidence has a unique ID and can be referenced by verification outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    /// Unique identifier for this evidence
    pub id: String,
    /// The type of evidence
    pub kind: EvidenceKind,
    /// The actual content of the evidence
    pub content: String,
    /// Source location (file:line, command, or other origin)
    pub source: String,
    /// Unix timestamp (ms) when evidence was collected
    pub timestamp: u64,
}

impl Evidence {
    /// Create new evidence.
    pub fn new(
        id: impl Into<String>,
        kind: EvidenceKind,
        content: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Self {
            id: id.into(),
            kind,
            content: content.into(),
            source: source.into(),
            timestamp,
        }
    }

    /// Create evidence from file content.
    pub fn from_file(path: impl Into<String>, content: impl Into<String>) -> Self {
        let path_str = path.into();
        let id = format!("file-{}", uuid::Uuid::new_v4());
        Self::new(
            id,
            EvidenceKind::FileContent {
                path: path_str.clone(),
            },
            content,
            path_str,
        )
    }

    /// Create evidence from command output.
    pub fn from_command(command: impl Into<String>, output: impl Into<String>) -> Self {
        let cmd_str = command.into();
        let id = format!("cmd-{}", uuid::Uuid::new_v4());
        Self::new(
            id,
            EvidenceKind::CommandOutput {
                command: cmd_str.clone(),
            },
            output,
            cmd_str,
        )
    }

    /// Create evidence from a test result.
    pub fn from_test(test_name: impl Into<String>, result: impl Into<String>) -> Self {
        let name_str = test_name.into();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        Self::new(
            id,
            EvidenceKind::TestResult {
                test_name: name_str.clone(),
            },
            result,
            name_str,
        )
    }

    /// Create evidence from a diff.
    pub fn from_diff(
        from: impl Into<String>,
        to: impl Into<String>,
        diff: impl Into<String>,
    ) -> Self {
        let from_str = from.into();
        let to_str = to.into();
        let id = format!("diff-{}", uuid::Uuid::new_v4());
        let kind = EvidenceKind::Diff {
            from: from_str.clone(),
            to: to_str.clone(),
        };
        let source = format!("{from_str} -> {to_str}");
        Self::new(id, kind, diff, source)
    }

    /// Get the kind name as a string.
    pub const fn kind_name(&self) -> &'static str {
        match &self.kind {
            EvidenceKind::FileContent { .. } => "file_content",
            EvidenceKind::CommandOutput { .. } => "command_output",
            EvidenceKind::TestResult { .. } => "test_result",
            EvidenceKind::Diff { .. } => "diff",
        }
    }
}

/// The type of evidence collected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceKind {
    /// Evidence from file contents
    FileContent {
        /// Path to the file
        path: String,
    },
    /// Evidence from command execution
    CommandOutput {
        /// The command that was run
        command: String,
    },
    /// Evidence from test execution
    TestResult {
        /// Name of the test
        test_name: String,
    },
    /// Evidence from a diff between two states
    Diff {
        /// The "from" state (e.g., original file)
        from: String,
        /// The "to" state (e.g., modified file)
        to: String,
    },
}

impl EvidenceKind {
    /// Check if this evidence kind matches a requirement string.
    pub fn matches_requirement(&self, requirement: &str) -> bool {
        match self {
            Self::FileContent { path } => {
                requirement == "file" || requirement == "file_content" || path == requirement
            }
            Self::CommandOutput { command } => {
                requirement == "command"
                    || requirement == "command_output"
                    || command == requirement
            }
            Self::TestResult { test_name } => {
                requirement == "test" || requirement == "test_result" || test_name == requirement
            }
            Self::Diff { from, to } => {
                requirement == "diff" || format!("{from} -> {to}") == requirement
            }
        }
    }
}

/// Gate that must be passed before completion is allowed.
///
/// Completion gates enforce that certain conditions are met before a task
/// can be marked as complete. This prevents premature completion claims.
#[derive(Debug, Clone, Default)]
pub struct CompletionGate {
    /// Whether all TODO items must be resolved
    pub todos_required: bool,
    /// Whether verification tests must pass
    pub tests_required: bool,
    /// Specific evidence kinds that must be present
    pub evidence_required: Vec<String>,
}

impl CompletionGate {
    /// Create a new completion gate with default settings (no requirements).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a strict completion gate requiring everything.
    pub fn strict() -> Self {
        Self {
            todos_required: true,
            tests_required: true,
            evidence_required: vec!["file".to_string(), "test".to_string()],
        }
    }

    /// Create a completion gate that only requires tests.
    pub fn tests_only() -> Self {
        Self {
            todos_required: false,
            tests_required: true,
            evidence_required: vec!["test".to_string()],
        }
    }

    /// Create a completion gate that only requires todos to be resolved.
    pub const fn todos_only() -> Self {
        Self {
            todos_required: true,
            tests_required: false,
            evidence_required: Vec::new(),
        }
    }

    /// Add a required evidence kind.
    pub fn require_evidence(mut self, kind: impl Into<String>) -> Self {
        let kind = kind.into();
        if !self.evidence_required.contains(&kind) {
            self.evidence_required.push(kind);
        }
        self
    }

    /// Set whether todos are required.
    pub const fn with_todos_required(mut self, required: bool) -> Self {
        self.todos_required = required;
        self
    }

    /// Set whether tests are required.
    pub const fn with_tests_required(mut self, required: bool) -> Self {
        self.tests_required = required;
        self
    }

    /// Check completion against this gate.
    pub fn check(
        &self,
        open_todos: usize,
        tests_passed: bool,
        evidence: &[Evidence],
    ) -> std::result::Result<(), CompletionError> {
        // Check TODO requirement
        if self.todos_required && open_todos > 0 {
            return Err(CompletionError::OpenTodosRemain { count: open_todos });
        }

        // Check test requirement
        if self.tests_required && !tests_passed {
            return Err(CompletionError::VerificationNotPassed);
        }

        // Check evidence requirements
        for required in &self.evidence_required {
            let found = evidence
                .iter()
                .any(|e| e.kind.matches_requirement(required));
            if !found {
                return Err(CompletionError::MissingEvidence {
                    kind: required.clone(),
                });
            }
        }

        Ok(())
    }
}

/// Errors that can occur when checking completion gates.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CompletionError {
    /// There are still open TODO items
    #[error("Cannot complete: {count} open TODO item(s) remain")]
    OpenTodosRemain {
        /// Number of open TODOs
        count: usize,
    },

    /// Verification tests have not passed
    #[error("Cannot complete: verification tests have not passed")]
    VerificationNotPassed,

    /// Required evidence is missing
    #[error("Cannot complete: missing required evidence of type '{kind}'")]
    MissingEvidence {
        /// The kind of evidence that is missing
        kind: String,
    },
}

/// Thread-safe store for evidence items.
#[derive(Debug, Clone)]
pub struct EvidenceStore {
    inner: Arc<Mutex<EvidenceStoreInner>>,
}

#[derive(Debug, Default)]
struct EvidenceStoreInner {
    evidence: HashMap<String, Evidence>,
    by_kind: HashMap<String, Vec<String>>,
}

impl EvidenceStore {
    /// Create a new empty evidence store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EvidenceStoreInner::default())),
        }
    }

    /// Add evidence to the store.
    pub fn add(&self, evidence: Evidence) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Error::Session(format!("Evidence store lock poisoned: {e}")))?;

        let id = evidence.id.clone();
        let kind_name = evidence.kind_name().to_string();

        inner.evidence.insert(id.clone(), evidence);
        inner.by_kind.entry(kind_name).or_default().push(id);

        Ok(())
    }

    /// Get evidence by ID.
    pub fn get(&self, id: &str) -> Option<Evidence> {
        let inner = self.inner.lock().ok()?;
        inner.evidence.get(id).cloned()
    }

    /// Get all evidence of a specific kind.
    pub fn get_by_kind(&self, kind: &str) -> Vec<Evidence> {
        let inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };

        match inner.by_kind.get(kind) {
            Some(ids) => ids
                .iter()
                .filter_map(|id| inner.evidence.get(id).cloned())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Get all evidence.
    pub fn all(&self) -> Vec<Evidence> {
        let inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };
        inner.evidence.values().cloned().collect()
    }

    /// Check if evidence of a specific kind exists.
    pub fn has_kind(&self, kind: &str) -> bool {
        let inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return false,
        };

        inner.by_kind.contains_key(kind) && !inner.by_kind[kind].is_empty()
    }

    /// Get the count of evidence items.
    pub fn len(&self) -> usize {
        let inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return 0,
        };
        inner.evidence.len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all evidence.
    pub fn clear(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| Error::Session(format!("Evidence store lock poisoned: {e}")))?;
        inner.evidence.clear();
        inner.by_kind.clear();
        Ok(())
    }

    /// Get evidence IDs for a list of IDs, returning only those that exist.
    pub fn get_existing_ids(&self, ids: &[String]) -> Vec<String> {
        let inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };
        ids.iter()
            .filter(|id| inner.evidence.contains_key(*id))
            .cloned()
            .collect()
    }

    /// Export evidence as a JSON-serializable format.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| Error::Session(format!("Evidence store lock poisoned: {e}")))?;
        serde_json::to_value(&inner.evidence).map_err(|e| Error::Json(Box::new(e)))
    }
}

impl Default for EvidenceStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Collector for task completion evidence.
///
/// Provides convenience methods for collecting common types of evidence
/// during task execution, such as test results, git diffs, and build results.
#[derive(Debug, Clone)]
pub struct EvidenceCollector {
    /// Collected evidence items.
    evidence: Vec<super::super::state::Evidence>,
    /// Whether to auto-collect evidence when methods are called.
    auto_collect: bool,
}

impl EvidenceCollector {
    /// Creates a new evidence collector.
    pub const fn new() -> Self {
        Self {
            evidence: Vec::new(),
            auto_collect: true,
        }
    }

    /// Creates a new evidence collector with specified auto-collection mode.
    pub const fn with_auto_collect(auto: bool) -> Self {
        Self {
            evidence: Vec::new(),
            auto_collect: auto,
        }
    }

    /// Collects test evidence using the default test command.
    pub fn collect_test_evidence(&mut self) -> Result<super::super::state::Evidence> {
        self.collect_test_evidence_with_command("cargo test --lib")
    }

    /// Collects test evidence by running the provided test command.
    ///
    /// Parses test output to extract passed/failed counts.
    pub fn collect_test_evidence_with_command(
        &mut self,
        command: &str,
    ) -> Result<super::super::state::Evidence> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .map_err(|e| Error::Tool {
                tool: command.to_string(),
                message: format!("Failed to execute: {e}"),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Parse test output for passed/failed counts
        // This handles common test output formats (cargo test, pytest, jest)
        let (passed, failed) = self.parse_test_output(&stdout, &stderr);

        let evidence = super::super::state::Evidence::TestOutput {
            passed,
            failed,
            output: format!("{stdout}\n{stderr}"),
        };

        if self.auto_collect {
            self.add(evidence.clone());
        }

        Ok(evidence)
    }

    /// Collects git diff evidence.
    pub fn collect_git_evidence(&mut self) -> Result<super::super::state::Evidence> {
        let output = Command::new("git")
            .args(["diff", "--stat"])
            .output()
            .map_err(|e| Error::Tool {
                tool: "git".to_string(),
                message: format!("Failed to get git diff: {e}"),
            })?;

        let stat_output = String::from_utf8_lossy(&output.stdout).to_string();

        // Parse files changed from stat output
        let files_changed = self.parse_files_changed(&stat_output);

        // Get full diff
        let diff_output = Command::new("git")
            .args(["diff"])
            .output()
            .map_err(|e| Error::Tool {
                tool: "git".to_string(),
                message: format!("Failed to get git diff: {e}"),
            })?;

        let diff = String::from_utf8_lossy(&diff_output.stdout).to_string();

        let evidence = super::super::state::Evidence::GitDiff {
            files_changed,
            diff,
        };

        if self.auto_collect {
            self.add(evidence.clone());
        }

        Ok(evidence)
    }

    /// Collects build evidence using the default build command.
    pub fn collect_build_evidence(&mut self) -> Result<super::super::state::Evidence> {
        self.collect_build_evidence_with_command("cargo build")
    }

    /// Collects build evidence by running the provided build command.
    pub fn collect_build_evidence_with_command(
        &mut self,
        command: &str,
    ) -> Result<super::super::state::Evidence> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .map_err(|e| Error::Tool {
                tool: command.to_string(),
                message: format!("Failed to execute build: {e}"),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        let success = output.status.success();
        let warnings = self.parse_warnings(&stdout, &stderr);

        let evidence = super::super::state::Evidence::BuildResult { success, warnings };

        if self.auto_collect {
            self.add(evidence.clone());
        }

        Ok(evidence)
    }

    /// Adds custom evidence to the collector.
    pub fn add(&mut self, evidence: super::super::state::Evidence) {
        self.evidence.push(evidence);
    }

    /// Gets all collected evidence.
    pub fn evidence(&self) -> &[super::super::state::Evidence] {
        &self.evidence
    }

    /// Checks if any evidence was collected.
    pub fn has_evidence(&self) -> bool {
        !self.evidence.is_empty()
    }

    /// Clears all collected evidence.
    pub fn clear(&mut self) {
        self.evidence.clear();
    }

    /// Returns the number of evidence items collected.
    pub fn len(&self) -> usize {
        self.evidence.len()
    }

    /// Returns true if no evidence has been collected.
    pub fn is_empty(&self) -> bool {
        self.evidence.is_empty()
    }

    /// Consumes the collector and returns all evidence.
    pub fn into_evidence(self) -> Vec<super::super::state::Evidence> {
        self.evidence
    }

    /// Parses test output to extract passed/failed counts.
    fn parse_test_output(&self, stdout: &str, stderr: &str) -> (usize, usize) {
        let combined = format!("{stdout} {stderr}");

        // Try to parse cargo test output
        // Format: "test result: ok. X passed; Y failed"
        if combined.contains("test result:") {
            let mut passed = 0;
            let mut failed = 0;

            // Extract passed count
            if let Some(idx) = combined.find("passed") {
                let before = &combined[..idx];
                if let Some(num) = before.split_whitespace().last() {
                    if let Ok(n) = num.trim_end_matches(',').parse::<usize>() {
                        passed = n;
                    }
                }
            }

            // Extract failed count
            if let Some(idx) = combined.find("failed") {
                let before = &combined[..idx];
                if let Some(num) = before.split_whitespace().last() {
                    if let Ok(n) = num.trim_end_matches(',').parse::<usize>() {
                        failed = n;
                    }
                }
            }

            return (passed, failed);
        }

        // Try to parse pytest output
        // Format: "X passed, Y failed"
        if combined.contains("passed") || combined.contains("failed") {
            let mut passed = 0;
            let mut failed = 0;

            for line in combined.lines() {
                if line.contains("passed") {
                    for part in line.split_whitespace() {
                        if let Ok(n) = part.parse::<usize>() {
                            passed = n;
                        }
                    }
                }
                if line.contains("failed") {
                    for part in line.split(',') {
                        if part.contains("failed") {
                            if let Some(num) = part.split_whitespace().next() {
                                if let Ok(n) = num.parse::<usize>() {
                                    failed = n;
                                }
                            }
                        }
                    }
                }
            }

            return (passed, failed);
        }

        // Default: assume success if exit code would be 0, else failure
        (0, 0)
    }

    /// Parses number of files changed from git diff --stat output.
    fn parse_files_changed(&self, stat_output: &str) -> usize {
        for line in stat_output.lines() {
            let trimmed = line.trim();
            if trimmed.ends_with("file changed") || trimmed.ends_with("files changed") {
                if let Some(count) = trimmed
                    .split_whitespace()
                    .next()
                    .and_then(|part| part.parse::<usize>().ok())
                {
                    return count;
                }
            }
        }

        stat_output
            .lines()
            .filter(|line| line.contains('|'))
            .count()
    }

    /// Parses warning count from build output.
    fn parse_warnings(&self, stdout: &str, stderr: &str) -> usize {
        let combined = format!("{stdout} {stderr}");

        // Common warning patterns

        combined.matches("warning:").count()
            + combined.matches("Warning:").count()
            + combined.matches("[WARNING]").count()
    }
}

impl Default for EvidenceCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_from_file() {
        let evidence = Evidence::from_file("/src/main.rs", "fn main() {}");
        assert!(evidence.id.starts_with("file-"));
        assert_eq!(evidence.kind_name(), "file_content");
        assert!(matches!(evidence.kind, EvidenceKind::FileContent { .. }));
    }

    #[test]
    fn evidence_from_command() {
        let evidence = Evidence::from_command("cargo test", "All tests passed");
        assert!(evidence.id.starts_with("cmd-"));
        assert_eq!(evidence.kind_name(), "command_output");
    }

    #[test]
    fn evidence_from_test() {
        let evidence = Evidence::from_test("test_foo", "passed");
        assert!(evidence.id.starts_with("test-"));
        assert_eq!(evidence.kind_name(), "test_result");
    }

    #[test]
    fn evidence_from_diff() {
        let evidence = Evidence::from_diff("a.txt", "b.txt", "-old\n+new");
        assert!(evidence.id.starts_with("diff-"));
        assert_eq!(evidence.kind_name(), "diff");
    }

    #[test]
    fn evidence_kind_matches_requirement() {
        let file_kind = EvidenceKind::FileContent {
            path: "src/main.rs".to_string(),
        };
        assert!(file_kind.matches_requirement("file"));
        assert!(file_kind.matches_requirement("file_content"));
        assert!(file_kind.matches_requirement("src/main.rs"));
        assert!(!file_kind.matches_requirement("command"));

        let test_kind = EvidenceKind::TestResult {
            test_name: "test_foo".to_string(),
        };
        assert!(test_kind.matches_requirement("test"));
        assert!(test_kind.matches_requirement("test_result"));
        assert!(test_kind.matches_requirement("test_foo"));
    }

    #[test]
    fn completion_gate_default() {
        let gate = CompletionGate::new();
        assert!(!gate.todos_required);
        assert!(!gate.tests_required);
        assert!(gate.evidence_required.is_empty());

        // Should pass with any state
        gate.check(5, false, &[]).unwrap();
    }

    #[test]
    fn completion_gate_strict() {
        let gate = CompletionGate::strict();
        assert!(gate.todos_required);
        assert!(gate.tests_required);
        assert!(!gate.evidence_required.is_empty());

        // Should fail with open todos
        let err = gate.check(1, true, &[]).unwrap_err();
        assert!(matches!(err, CompletionError::OpenTodosRemain { count: 1 }));

        // Should fail without tests passing
        let err = gate.check(0, false, &[]).unwrap_err();
        assert!(matches!(err, CompletionError::VerificationNotPassed));
    }

    #[test]
    fn completion_gate_tests_only() {
        let gate = CompletionGate::tests_only();
        assert!(!gate.todos_required);
        assert!(gate.tests_required);

        // Need test evidence for the tests_only gate
        let test_evidence = Evidence::from_test("test_foo", "passed");

        // Should pass with tests passed and evidence, but open todos
        gate.check(5, true, &[test_evidence.clone()]).unwrap();

        // Should fail without tests passing
        let err = gate.check(0, false, &[test_evidence]).unwrap_err();
        assert!(matches!(err, CompletionError::VerificationNotPassed));
    }

    #[test]
    fn completion_gate_todos_only() {
        let gate = CompletionGate::todos_only();
        assert!(gate.todos_required);
        assert!(!gate.tests_required);

        // Should pass with no todos but no tests
        gate.check(0, false, &[]).unwrap();

        // Should fail with open todos
        let err = gate.check(3, false, &[]).unwrap_err();
        assert!(matches!(err, CompletionError::OpenTodosRemain { count: 3 }));
    }

    #[test]
    fn completion_gate_missing_evidence() {
        let gate = CompletionGate::new()
            .require_evidence("file")
            .require_evidence("test");

        let err = gate.check(0, true, &[]).unwrap_err();
        assert!(matches!(err, CompletionError::MissingEvidence { kind } if kind == "file"));

        let file_evidence = Evidence::from_file("src/main.rs", "content");
        let err = gate.check(0, true, &[file_evidence.clone()]).unwrap_err();
        assert!(matches!(err, CompletionError::MissingEvidence { kind } if kind == "test"));

        let test_evidence = Evidence::from_test("test_foo", "passed");
        gate.check(0, true, &[file_evidence, test_evidence])
            .unwrap();
    }

    #[test]
    fn evidence_store_basic() {
        let store = EvidenceStore::new();
        assert!(store.is_empty());

        let evidence = Evidence::from_file("test.rs", "fn test() {}");
        let id = evidence.id.clone();
        store.add(evidence).unwrap();

        assert_eq!(store.len(), 1);
        assert!(store.get(&id).is_some());
        assert!(store.has_kind("file_content"));

        let all = store.all();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn evidence_store_by_kind() {
        let store = EvidenceStore::new();

        store.add(Evidence::from_file("a.rs", "a")).unwrap();
        store.add(Evidence::from_file("b.rs", "b")).unwrap();
        store
            .add(Evidence::from_command("cargo test", "passed"))
            .unwrap();

        let files = store.get_by_kind("file_content");
        assert_eq!(files.len(), 2);

        let commands = store.get_by_kind("command_output");
        assert_eq!(commands.len(), 1);

        let tests = store.get_by_kind("test_result");
        assert!(tests.is_empty());
    }

    #[test]
    fn evidence_store_clear() {
        let store = EvidenceStore::new();
        store
            .add(Evidence::from_file("test.rs", "content"))
            .unwrap();
        assert!(!store.is_empty());

        store.clear().unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn evidence_store_to_json() {
        let store = EvidenceStore::new();
        store
            .add(Evidence::from_file("test.rs", "fn test() {}"))
            .unwrap();

        let json = store.to_json().unwrap();
        assert!(json.is_object());
    }

    // EvidenceCollector tests

    #[test]
    fn evidence_collector_new() {
        let collector = EvidenceCollector::new();
        assert!(collector.is_empty());
        assert!(!collector.has_evidence());
        assert_eq!(collector.len(), 0);
    }

    #[test]
    fn evidence_collector_with_auto_collect() {
        let collector = EvidenceCollector::with_auto_collect(false);
        assert!(!collector.auto_collect);
    }

    #[test]
    fn evidence_collector_add() {
        let mut collector = EvidenceCollector::new();

        // The EvidenceCollector uses crate::state::Evidence internally.
        use crate::state::Evidence as StateEvidence;
        collector.add(StateEvidence::TestOutput {
            passed: 5,
            failed: 0,
            output: "All tests passed".to_string(),
        });

        assert!(collector.has_evidence());
        assert_eq!(collector.len(), 1);
    }

    #[test]
    fn evidence_collector_clear() {
        let mut collector = EvidenceCollector::new();

        use crate::state::Evidence as StateEvidence;
        collector.add(StateEvidence::TestOutput {
            passed: 1,
            failed: 0,
            output: "test".to_string(),
        });

        assert!(!collector.is_empty());
        collector.clear();
        assert!(collector.is_empty());
    }

    #[test]
    fn evidence_collector_into_evidence() {
        let mut collector = EvidenceCollector::new();

        use crate::state::Evidence as StateEvidence;
        collector.add(StateEvidence::TestOutput {
            passed: 2,
            failed: 0,
            output: "test".to_string(),
        });

        let evidence = collector.into_evidence();
        assert_eq!(evidence.len(), 1);
    }

    #[test]
    fn evidence_collector_parse_test_output_cargo() {
        let collector = EvidenceCollector::new();

        let stdout = "running 5 tests\ntest src/lib.rs - test_foo (line 10) ... ok\ntest result: ok. 5 passed; 0 failed";
        let stderr = "";

        let (passed, failed) = collector.parse_test_output(stdout, stderr);
        assert_eq!(passed, 5);
        assert_eq!(failed, 0);
    }

    #[test]
    fn evidence_collector_parse_test_output_with_failures() {
        let collector = EvidenceCollector::new();

        let stdout = "running 10 tests\ntest result: FAILED. 8 passed; 2 failed";
        let stderr = "";

        let (passed, failed) = collector.parse_test_output(stdout, stderr);
        assert_eq!(passed, 8);
        assert_eq!(failed, 2);
    }

    #[test]
    fn evidence_collector_parse_files_changed() {
        let collector = EvidenceCollector::new();

        let stat_output =
            " src/main.rs | 10 +++++-----\n src/lib.rs   |  5 +++--\n 2 files changed";
        let count = collector.parse_files_changed(stat_output);
        assert_eq!(count, 2);
    }

    #[test]
    fn evidence_collector_parse_warnings() {
        let collector = EvidenceCollector::new();

        let stdout = "warning: unused variable\nwarning: dead code";
        let stderr = "";

        let warnings = collector.parse_warnings(stdout, stderr);
        assert_eq!(warnings, 2);
    }

    #[test]
    fn evidence_collector_parse_warnings_case_insensitive() {
        let collector = EvidenceCollector::new();

        let stdout = "Warning: something\n[WARNING] another thing";
        let stderr = "";

        let warnings = collector.parse_warnings(stdout, stderr);
        assert_eq!(warnings, 2);
    }
}
