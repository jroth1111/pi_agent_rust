//! Test quality gate implementation.
//!
//! This module provides a completion gate that checks test quality
//! using the test analyzer.

use super::test_analyzer::{TestAnalyzer, TestQualityIssue};
use crate::state::{CompletionGate, GateResult, GateSeverity};
use std::path::PathBuf;

/// Gate that checks test quality.
pub struct TestQualityGate {
    test_paths: Vec<PathBuf>,
    severity: GateSeverity,
    /// Require assertions in test functions.
    pub require_assertions: bool,
    /// Require meaningful (non-tautological) assertions.
    pub require_meaningful_assertions: bool,
    /// Maximum number of trivial assertions tolerated before failing.
    pub max_trivial_assertions: usize,
}

impl TestQualityGate {
    /// Creates a new test quality gate with default settings.
    pub const fn new(test_paths: Vec<PathBuf>) -> Self {
        Self {
            test_paths,
            severity: GateSeverity::Required,
            require_assertions: true,
            require_meaningful_assertions: true,
            max_trivial_assertions: 0,
        }
    }

    /// Creates a new gate with specified severity.
    pub const fn with_severity(severity: GateSeverity, test_paths: Vec<PathBuf>) -> Self {
        Self {
            test_paths,
            severity,
            require_assertions: true,
            require_meaningful_assertions: true,
            max_trivial_assertions: 0,
        }
    }

    /// Creates a new gate with a custom analyzer.
    pub const fn with_analyzer(analyzer: TestAnalyzer, test_paths: Vec<PathBuf>) -> Self {
        Self {
            test_paths,
            severity: GateSeverity::Required,
            require_assertions: analyzer.require_assertions(),
            require_meaningful_assertions: analyzer.require_meaningful_assertions(),
            max_trivial_assertions: analyzer.max_trivial_assertions(),
        }
    }

    /// Creates a new gate with explicit quality requirements.
    pub const fn with_requirements(
        test_paths: Vec<PathBuf>,
        require_assertions: bool,
        require_meaningful_assertions: bool,
        max_trivial_assertions: usize,
    ) -> Self {
        Self {
            test_paths,
            severity: GateSeverity::Required,
            require_assertions,
            require_meaningful_assertions,
            max_trivial_assertions,
        }
    }

    /// Returns the test paths being checked.
    pub fn test_paths(&self) -> &[PathBuf] {
        &self.test_paths
    }
}

impl CompletionGate for TestQualityGate {
    fn name(&self) -> &'static str {
        "test-quality"
    }

    fn description(&self) -> &'static str {
        "Checks for tautological or low-quality tests"
    }

    fn severity(&self) -> GateSeverity {
        self.severity
    }

    fn check(&self) -> crate::error::Result<GateResult> {
        use std::time::Instant;

        let analyzer = TestAnalyzer::with_requirements(
            self.require_assertions,
            self.require_meaningful_assertions,
            self.max_trivial_assertions,
        );

        let start = Instant::now();
        let mut all_issues = Vec::new();
        let mut files_checked = 0;
        let mut total_lines = 0;

        for test_path in &self.test_paths {
            // Check if path exists and is a file or directory
            if !test_path.exists() {
                continue;
            }

            if test_path.is_file() {
                if let Ok(source) = std::fs::read_to_string(test_path) {
                    total_lines += source.lines().count();
                    let issues = analyzer.analyze(&source);
                    for issue in issues {
                        all_issues.push((test_path.clone(), issue));
                    }
                    files_checked += 1;
                }
            } else if test_path.is_dir() {
                // Recursively find test files
                if let Ok(entries) = std::fs::read_dir(test_path) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_file() {
                            let ext = path.extension().and_then(|s| s.to_str());
                            if ext == Some("rs") {
                                if let Ok(source) = std::fs::read_to_string(&path) {
                                    total_lines += source.lines().count();
                                    let issues = analyzer.analyze(&source);
                                    for issue in issues {
                                        all_issues.push((path.clone(), issue));
                                    }
                                    files_checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        let duration = start.elapsed();

        if all_issues.is_empty() {
            Ok(GateResult::passed_with_duration(
                format!("All {files_checked} test files passed quality checks"),
                duration,
            ))
        } else {
            let score = calculate_quality_score(
                &all_issues.iter().map(|(_, i)| i).collect::<Vec<_>>(),
                total_lines,
            );
            let issue_summary = format_issue_summary(&all_issues);
            Ok(GateResult::failed_with_duration(
                format!(
                    "Test quality issues found in {} file(s). Score: {:.1}/100. {}",
                    files_checked,
                    score * 100.0,
                    issue_summary
                ),
                duration,
            ))
        }
    }
}

/// Formats a summary of issues for display.
fn format_issue_summary(issues: &[(PathBuf, TestQualityIssue)]) -> usize {
    let mut no_assertions = 0;
    let mut trivial = 0;
    let mut empty = 0;
    let mut no_calls = 0;
    let mut unreachable = 0;

    for (_, issue) in issues {
        match issue {
            TestQualityIssue::NoAssertions { .. } => no_assertions += 1,
            TestQualityIssue::TrivialAssertion { .. } => trivial += 1,
            TestQualityIssue::EmptyTest { .. } => empty += 1,
            TestQualityIssue::NoTestedFunction { .. } => no_calls += 1,
            TestQualityIssue::UnreachableCode { .. } => unreachable += 1,
        }
    }

    no_assertions + trivial + empty + no_calls + unreachable
}

/// Calculate a quality score (0.0 to 1.0) for test files based on issues.
///
/// The score is calculated as:
/// - Base score: 1.0
/// - Penalty per issue: 0.05 (5%)
/// - Minimum score: 0.0
///
/// This is a simple heuristic; in practice you might want to weight
/// different issue types differently.
pub fn calculate_quality_score(issues: &[&TestQualityIssue], total_lines: usize) -> f32 {
    if total_lines == 0 {
        return 1.0;
    }

    // Base score
    let mut score = 1.0_f32;

    // Penalty weights per issue type
    let no_assertions_penalty = 0.15;
    let trivial_penalty = 0.05;
    let empty_penalty = 0.20;
    let no_calls_penalty = 0.10;
    let unreachable_penalty = 0.03;

    for issue in issues {
        let penalty = match issue {
            TestQualityIssue::NoAssertions { .. } => no_assertions_penalty,
            TestQualityIssue::TrivialAssertion { .. } => trivial_penalty,
            TestQualityIssue::EmptyTest { .. } => empty_penalty,
            TestQualityIssue::NoTestedFunction { .. } => no_calls_penalty,
            TestQualityIssue::UnreachableCode { .. } => unreachable_penalty,
        };
        score = (score - penalty).max(0.0);
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_quality_score_perfect() {
        let score = calculate_quality_score(&[], 100);
        assert!((score - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_calculate_quality_score_with_issues() {
        let trivial_issue = TestQualityIssue::TrivialAssertion {
            line: 15,
            text: "assert!(true)".to_string(),
        };
        let issues = vec![&TestQualityIssue::NoAssertions { line: 10 }, &trivial_issue];
        let score = calculate_quality_score(&issues, 100);
        assert!(score < 1.0);
        assert!(score > 0.5);
    }

    #[test]
    fn test_calculate_quality_score_many_issues() {
        let issues = vec![
            &TestQualityIssue::EmptyTest { line: 1 },
            &TestQualityIssue::EmptyTest { line: 10 },
            &TestQualityIssue::EmptyTest { line: 20 },
            &TestQualityIssue::EmptyTest { line: 30 },
            &TestQualityIssue::EmptyTest { line: 40 },
            &TestQualityIssue::EmptyTest { line: 50 },
        ];
        let score = calculate_quality_score(&issues, 100);
        assert!(
            (score - 0.0).abs() < f32::EPSILON,
            "Score should be 0 with 6 empty tests (6 * 0.20 = 1.2)"
        );
    }

    #[test]
    fn test_calculate_quality_score_zero_lines() {
        let issues = vec![&TestQualityIssue::EmptyTest { line: 1 }];
        let score = calculate_quality_score(&issues, 0);
        assert!(
            (score - 1.0).abs() < f32::EPSILON,
            "Zero lines should return perfect score"
        );
    }

    #[test]
    fn test_gate_name_and_description() {
        let gate = TestQualityGate::new(vec![PathBuf::from("tests/test.rs")]);
        assert_eq!(gate.name(), "test-quality");
        assert_eq!(
            gate.description(),
            "Checks for tautological or low-quality tests"
        );
        assert_eq!(gate.severity(), GateSeverity::Required);
    }

    #[test]
    fn test_gate_with_severity() {
        let gate = TestQualityGate::with_severity(
            GateSeverity::Recommended,
            vec![PathBuf::from("tests/test.rs")],
        );
        assert_eq!(gate.severity(), GateSeverity::Recommended);
    }

    #[test]
    fn test_gate_with_custom_analyzer() {
        let analyzer = TestAnalyzer::with_requirements(false, false, 10);
        let gate = TestQualityGate::with_analyzer(analyzer, vec![PathBuf::from("tests/test.rs")]);
        assert_eq!(gate.test_paths().len(), 1);
        assert!(!gate.require_assertions);
        assert!(!gate.require_meaningful_assertions);
        assert_eq!(gate.max_trivial_assertions, 10);
    }

    #[test]
    fn test_gate_with_requirements() {
        let gate =
            TestQualityGate::with_requirements(vec![PathBuf::from("tests/test.rs")], true, true, 2);
        assert!(gate.require_assertions);
        assert!(gate.require_meaningful_assertions);
        assert_eq!(gate.max_trivial_assertions, 2);
    }

    #[test]
    fn test_gate_passes_with_no_files() {
        let gate = TestQualityGate::new(vec![]);
        let result = gate.check().unwrap();
        assert!(result.is_pass());
    }

    #[test]
    fn test_gate_passes_with_nonexistent_path() {
        let gate = TestQualityGate::new(vec![PathBuf::from("/nonexistent/path")]);
        let result = gate.check().unwrap();
        assert!(result.is_pass());
    }

    #[test]
    fn test_format_issue_summary_counts() {
        let issues = vec![
            (
                PathBuf::from("test.rs"),
                TestQualityIssue::NoAssertions { line: 10 },
            ),
            (
                PathBuf::from("test.rs"),
                TestQualityIssue::EmptyTest { line: 20 },
            ),
            (
                PathBuf::from("test.rs"),
                TestQualityIssue::TrivialAssertion {
                    line: 30,
                    text: "assert!(true)".to_string(),
                },
            ),
        ];
        let count = format_issue_summary(&issues);
        assert_eq!(count, 3);
    }

    #[test]
    fn test_issue_penalty_weights() {
        // Empty test should have highest penalty
        let issues = vec![&TestQualityIssue::EmptyTest { line: 1 }];
        let score_empty = calculate_quality_score(&issues, 100);
        assert!(
            (score_empty - 0.8).abs() < f32::EPSILON,
            "Empty test should have 0.20 penalty"
        );

        // Trivial assertion should have moderate penalty
        let trivial_issue = TestQualityIssue::TrivialAssertion {
            line: 1,
            text: "assert!(true)".to_string(),
        };
        let issues = vec![&trivial_issue];
        let score_trivial = calculate_quality_score(&issues, 100);
        assert!(
            (score_trivial - 0.95).abs() < f32::EPSILON,
            "Trivial assertion should have 0.05 penalty"
        );

        // No assertions should have high penalty
        let issues = vec![&TestQualityIssue::NoAssertions { line: 1 }];
        let score_no_assert = calculate_quality_score(&issues, 100);
        assert!(
            (score_no_assert - 0.85).abs() < f32::EPSILON,
            "No assertions should have 0.15 penalty"
        );
    }

    #[test]
    fn test_gate_check_with_temp_file() {
        use std::io::Write;

        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("test_quality_test.rs");

        // Write a test file with issues
        let content = r"
#[test]
fn test_trivial() {
    assert!(true);
}

#[test]
fn test_empty() {
}

#[test]
fn test_good() {
    let result = 2 + 2;
    assert_eq!(result, 4);
}
";

        let mut file = std::fs::File::create(&temp_file).unwrap();
        file.write_all(content.as_bytes()).unwrap();

        let gate = TestQualityGate::new(vec![temp_file.clone()]);
        let result = gate.check().unwrap();

        // Should fail because there are issues
        assert!(result.is_fail());
        assert!(result.message().contains("quality"));

        // Cleanup
        let _ = std::fs::remove_file(&temp_file);
    }
}
