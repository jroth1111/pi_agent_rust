//! Test file quality analyzer.
//!
//! This module provides static analysis capabilities for detecting common
//! test quality issues like tautological assertions, empty tests, and
//! unreachable code.

use serde::{Deserialize, Serialize};

/// Issue found in test quality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TestQualityIssue {
    /// Test has no assertions.
    NoAssertions { line: usize },
    /// Test has trivial assertion (always true).
    TrivialAssertion { line: usize, text: String },
    /// Unreachable code in test.
    UnreachableCode { line: usize },
    /// Test doesn't call any function.
    NoTestedFunction { line: usize },
    /// Empty test body.
    EmptyTest { line: usize },
}

impl std::fmt::Display for TestQualityIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAssertions { line } => write!(f, "No assertions found (line {line})"),
            Self::TrivialAssertion { line, text } => {
                write!(f, "Trivial assertion at line {line}: {text}")
            }
            Self::UnreachableCode { line } => write!(f, "Unreachable code at line {line}"),
            Self::NoTestedFunction { line } => {
                write!(f, "No function calls detected in test at line {line}")
            }
            Self::EmptyTest { line } => write!(f, "Empty test body at line {line}"),
        }
    }
}

/// Patterns that indicate trivial assertions.
const TRIVIAL_PATTERNS: &[&str] = &[
    "assert!(true)",
    "assert!(false == false)",
    "assert!(true == true)",
    "assert!(1 == 1)",
    "assert!(0 == 0)",
    "assert!(\"\" == \"\")",
    "assert!(const_true)",
    "assert_eq!(true, true)",
    "assert_eq!(false, false)",
    "assert_eq!(1, 1)",
    "assert_eq!(0, 0)",
    "assert_ne!(true, false)",
    "assert_ne!(1, 0)",
    "assert!(not(false))", // logic still trivial
];

/// Common assertion macro names.
const ASSERTION_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "assert_matches",
    "assert_ok",
    "assert_err",
    "assert_contains",
    "assert_approx_eq",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "expect",
    "should_panic",
];

/// Analyzer for test file quality.
#[derive(Debug, Clone)]
pub struct TestAnalyzer {
    require_assertions: bool,
    require_meaningful_assertions: bool,
    max_trivial_assertions: usize,
}

impl Default for TestAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl TestAnalyzer {
    /// Creates a new analyzer with default settings.
    pub const fn new() -> Self {
        Self {
            require_assertions: true,
            require_meaningful_assertions: true,
            max_trivial_assertions: 0,
        }
    }

    /// Creates an analyzer with custom requirements.
    pub const fn with_requirements(
        require_assertions: bool,
        require_meaningful: bool,
        max_trivial: usize,
    ) -> Self {
        Self {
            require_assertions,
            require_meaningful_assertions: require_meaningful,
            max_trivial_assertions: max_trivial,
        }
    }

    /// Whether assertions are required for each test.
    pub const fn require_assertions(&self) -> bool {
        self.require_assertions
    }

    /// Whether trivial assertions should be flagged.
    pub const fn require_meaningful_assertions(&self) -> bool {
        self.require_meaningful_assertions
    }

    /// Maximum trivial assertions allowed before issues are emitted.
    pub const fn max_trivial_assertions(&self) -> usize {
        self.max_trivial_assertions
    }

    /// Analyzes source code for test quality issues.
    pub fn analyze(&self, source: &str) -> Vec<TestQualityIssue> {
        let mut issues = Vec::new();

        // Parse the source line by line
        let lines: Vec<&str> = source.lines().collect();

        // Track test functions and their properties
        let mut in_test_fn = false;
        let mut test_fn_start = 0;
        let mut test_fn_has_assertion = false;
        let mut test_fn_has_call = false;
        let mut test_fn_body_lines = Vec::new();
        let mut brace_depth = 0;

        for (idx, line) in lines.iter().enumerate() {
            let line_num = idx + 1;
            let trimmed = line.trim();

            // Check for test function definition
            if trimmed.starts_with("#[test]") || trimmed.starts_with("#test") {
                // Next non-empty line should be fn declaration
                continue;
            }

            // Check for function declaration (simple heuristic)
            if trimmed.starts_with("fn ") && trimmed.ends_with('{') {
                // Check if this looks like a test function (has 'test' in name or preceded by test attribute)
                if in_test_fn {
                    // Previous test function wasn't closed properly, analyze it
                    issues.extend(self.analyze_test_function(
                        test_fn_start,
                        &test_fn_body_lines,
                        test_fn_has_assertion,
                        test_fn_has_call,
                    ));
                }

                // Reset for new function
                in_test_fn = true;
                test_fn_start = line_num;
                test_fn_has_assertion = false;
                test_fn_has_call = false;
                test_fn_body_lines.clear();
                brace_depth = 1;
                continue;
            }

            // Track braces for function scope
            brace_depth += trimmed.matches('{').count() as i32;
            brace_depth -= trimmed.matches('}').count() as i32;

            // If we're in a test function, analyze its body
            if in_test_fn && brace_depth > 0 {
                test_fn_body_lines.push((line_num, trimmed));

                // Check for assertions
                for macro_name in ASSERTION_MACROS {
                    if trimmed.contains(macro_name) && trimmed.contains('!') {
                        test_fn_has_assertion = true;

                        // Check if it's a trivial assertion
                        if self.require_meaningful_assertions {
                            if let Some(trivial) = self.check_trivial_assertion(trimmed) {
                                issues.push(TestQualityIssue::TrivialAssertion {
                                    line: line_num,
                                    text: trivial,
                                });
                            }
                        }
                    }
                }

                let is_assertion_macro = ASSERTION_MACROS
                    .iter()
                    .any(|macro_name| trimmed.contains(macro_name) && trimmed.contains('!'));

                // Check for function calls (heuristic: contains '(' and not assertion macro)
                if trimmed.contains('(') && !trimmed.starts_with("//") && !is_assertion_macro {
                    // Look for identifier followed by '(' that's not a macro
                    let call_pattern = trimmed.matches('(').count();
                    if call_pattern > 0 || trimmed.contains('.') {
                        // Exclude control structures
                        if !trimmed.starts_with("if ")
                            && !trimmed.starts_with("while ")
                            && !trimmed.starts_with("for ")
                            && !trimmed.starts_with("match ")
                        {
                            test_fn_has_call = true;
                        }
                    }
                }

                // Check for unreachable code
                if trimmed.contains("panic!") && trimmed.contains("unreachable") {
                    issues.push(TestQualityIssue::UnreachableCode { line: line_num });
                }
                if trimmed.contains("unreachable!") {
                    issues.push(TestQualityIssue::UnreachableCode { line: line_num });
                }
            }

            // Function closed
            if in_test_fn && brace_depth <= 0 {
                issues.extend(self.analyze_test_function(
                    test_fn_start,
                    &test_fn_body_lines,
                    test_fn_has_assertion,
                    test_fn_has_call,
                ));
                in_test_fn = false;
            }
        }

        // Handle test function that extends to end of file
        if in_test_fn {
            issues.extend(self.analyze_test_function(
                test_fn_start,
                &test_fn_body_lines,
                test_fn_has_assertion,
                test_fn_has_call,
            ));
        }

        if self.require_meaningful_assertions {
            let mut trivial_seen = 0usize;
            issues.retain(|issue| match issue {
                TestQualityIssue::TrivialAssertion { .. } => {
                    trivial_seen = trivial_seen.saturating_add(1);
                    trivial_seen > self.max_trivial_assertions
                }
                _ => true,
            });
        }

        issues
    }

    /// Analyzes a single test function for quality issues.
    fn analyze_test_function(
        &self,
        start_line: usize,
        body_lines: &[(usize, &str)],
        has_assertion: bool,
        has_call: bool,
    ) -> Vec<TestQualityIssue> {
        let mut issues = Vec::new();

        // Filter out comments and empty lines for body analysis
        let non_empty: Vec<&str> = body_lines
            .iter()
            .filter(|(_, line)| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with("//")
            })
            .map(|(_, line)| *line)
            .collect();

        // Check for empty test
        if non_empty.is_empty() {
            issues.push(TestQualityIssue::EmptyTest { line: start_line });
        }

        // Check for missing assertions
        if self.require_assertions && !has_assertion {
            issues.push(TestQualityIssue::NoAssertions { line: start_line });
        }

        // Check for no tested function (no calls made)
        if !has_call && non_empty.len() > 1 {
            // Allow 1 line (just the assertion) but more than that without calls is suspicious
            issues.push(TestQualityIssue::NoTestedFunction { line: start_line });
        }

        issues
    }

    /// Checks if an assertion is trivial/tautological.
    fn check_trivial_assertion(&self, line: &str) -> Option<String> {
        let normalized = line.trim().replace(' ', "");

        for pattern in TRIVIAL_PATTERNS {
            if normalized.contains(&pattern.replace(' ', "")) {
                return Some(line.trim().to_string());
            }
        }

        // Check for numeric tautologies: 1 == 1, 0 == 0, etc.
        if normalized.contains("==") || normalized.contains("!=") {
            let parts: Vec<&str> = normalized.split(['=', '!', '(', ')', ',', ';']).collect();
            // Look for same value on both sides
            for window in parts.windows(2) {
                if !window[0].is_empty() && window[0] == window[1] {
                    if let Ok(num) = window[0].parse::<f64>() {
                        if num == 0.0 || num.abs() < 10.0 {
                            return Some(line.trim().to_string());
                        }
                    }
                    // String tautology
                    if window[0].starts_with('"') || window[0].starts_with('\'') {
                        return Some(line.trim().to_string());
                    }
                }
            }
        }

        None
    }

    /// Analyzes a test file for quality issues.
    pub fn analyze_file(
        &self,
        path: &std::path::Path,
    ) -> crate::error::Result<Vec<TestQualityIssue>> {
        let source =
            std::fs::read_to_string(path).map_err(|e| crate::error::Error::Io(Box::new(e)))?;
        Ok(self.analyze(&source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detects_no_assertions() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_without_assertion() {
    let x = 1 + 1;
}
";
        let issues = analyzer.analyze(source);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, TestQualityIssue::NoAssertions { .. }))
        );
    }

    #[test]
    fn test_detects_trivial_assertions() {
        let analyzer = TestAnalyzer::with_requirements(true, true, 0);
        let source = r"
#[test]
fn test_trivial() {
    assert!(true);
    assert!(1 == 1);
}
";
        let issues = analyzer.analyze(source);
        let trivial_count = issues
            .iter()
            .filter(|i| matches!(i, TestQualityIssue::TrivialAssertion { .. }))
            .count();
        assert_eq!(trivial_count, 2);
    }

    #[test]
    fn test_detects_empty_test() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_empty() {
}
";
        let issues = analyzer.analyze(source);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, TestQualityIssue::EmptyTest { .. }))
        );
    }

    #[test]
    fn test_detects_no_function_calls() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_no_calls() {
    let x = 5;
    let y = 10;
    assert_eq!(x, y);
}
";
        let issues = analyzer.analyze(source);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, TestQualityIssue::NoTestedFunction { .. }))
        );
    }

    #[test]
    fn test_detects_unreachable_code() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_unreachable() {
    unreachable!();
}
";
        let issues = analyzer.analyze(source);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, TestQualityIssue::UnreachableCode { .. }))
        );
    }

    #[test]
    fn test_passes_good_test() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_good() {
    let result = add(2, 3);
    assert_eq!(result, 5);
}
";
        let issues = analyzer.analyze(source);
        assert!(issues.is_empty(), "Expected no issues, got: {issues:?}");
    }

    #[test]
    fn test_with_disabled_assertion_requirement() {
        let analyzer = TestAnalyzer::with_requirements(false, false, 100);
        let source = r"
#[test]
fn test_no_assertion_ok() {
    let _x = 1;
}
";
        let issues = analyzer.analyze(source);
        // Should not have NoAssertions issue
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, TestQualityIssue::NoAssertions { .. }))
        );
    }

    #[test]
    fn test_detects_multiple_trivial_patterns() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_various_trivial() {
    assert!(true);
    assert!(false == false);
    assert_eq!(1, 1);
    assert_ne!(true, false);
}
";
        let issues = analyzer.analyze(source);
        let trivial_count = issues
            .iter()
            .filter(|i| matches!(i, TestQualityIssue::TrivialAssertion { .. }))
            .count();
        assert!(
            trivial_count >= 3,
            "Expected at least 3 trivial assertions, got {trivial_count}"
        );
    }

    #[test]
    fn test_issue_display() {
        let issue = TestQualityIssue::NoAssertions { line: 42 };
        assert_eq!(issue.to_string(), "No assertions found (line 42)");

        let issue = TestQualityIssue::TrivialAssertion {
            line: 10,
            text: "assert!(true)".to_string(),
        };
        assert_eq!(
            issue.to_string(),
            "Trivial assertion at line 10: assert!(true)"
        );
    }

    #[test]
    fn test_respects_max_trivial() {
        let analyzer = TestAnalyzer::with_requirements(true, true, 1);
        let source = r"
#[test]
fn test_some_trivial_allowed() {
    assert!(true);
    assert!(1 == 1);
    let x = do_something();
    assert_eq!(x, 42);
}
";
        let issues = analyzer.analyze(source);
        let trivial_count = issues
            .iter()
            .filter(|i| matches!(i, TestQualityIssue::TrivialAssertion { .. }))
            .count();
        assert_eq!(trivial_count, 1);
        assert_eq!(analyzer.max_trivial_assertions(), 1);
    }

    #[test]
    fn test_comments_ignored_in_empty_check() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_comments_only() {
    // This is a comment
    // Another comment
}
";
        let issues = analyzer.analyze(source);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, TestQualityIssue::EmptyTest { .. }))
        );
    }

    #[test]
    fn test_multiple_tests_in_file() {
        let analyzer = TestAnalyzer::new();
        let source = r"
#[test]
fn test_bad() {
    assert!(true);
}

#[test]
fn test_good() {
    let result = double(5);
    assert_eq!(result, 10);
}
";
        let issues = analyzer.analyze(source);
        // Should have at least one issue from the bad test
        assert!(!issues.is_empty());
    }
}
