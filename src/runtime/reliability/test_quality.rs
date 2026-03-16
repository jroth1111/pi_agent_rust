use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TautologyFinding {
    pub line: usize,
    pub pattern: String,
    pub message: String,
}

pub fn detect_tautological_test_patterns(source: &str) -> Vec<TautologyFinding> {
    let mut findings = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }

        if trimmed.contains("assert!(true)") {
            findings.push(TautologyFinding {
                line: line_no,
                pattern: "assert!(true)".to_string(),
                message: "Assertion is always true and does not validate behavior".to_string(),
            });
        }

        if trimmed.contains("assert_eq!(true, true)")
            || trimmed.contains("assert_eq!( false , false )")
        {
            findings.push(TautologyFinding {
                line: line_no,
                pattern: "assert_eq!(<literal>, <same literal>)".to_string(),
                message: "Equality assertion compares identical literals".to_string(),
            });
        }

        if let Some(finding) = detect_assert_eq_same_expr(trimmed, line_no) {
            findings.push(finding);
        }

        if let Some(finding) = detect_empty_test_body(trimmed, line_no) {
            findings.push(finding);
        }
    }
    findings
}

fn detect_assert_eq_same_expr(line: &str, line_no: usize) -> Option<TautologyFinding> {
    static ASSERT_EQ_RE: OnceLock<Regex> = OnceLock::new();
    let re = ASSERT_EQ_RE.get_or_init(|| {
        Regex::new(r"assert_eq!\(\s*([A-Za-z0-9_:.]+)\s*,\s*([A-Za-z0-9_:.]+)\s*\)")
            .expect("valid regex")
    });
    let captures = re.captures(line)?;
    let left = captures.get(1)?.as_str();
    let right = captures.get(2)?.as_str();
    if left != right {
        return None;
    }
    Some(TautologyFinding {
        line: line_no,
        pattern: "assert_eq!(x, x)".to_string(),
        message: "Assertion compares an expression to itself".to_string(),
    })
}

fn detect_empty_test_body(line: &str, line_no: usize) -> Option<TautologyFinding> {
    static EMPTY_TEST_RE: OnceLock<Regex> = OnceLock::new();
    let re = EMPTY_TEST_RE.get_or_init(|| {
        Regex::new(r"^fn\s+test_[A-Za-z0-9_]+\s*\(\s*\)\s*\{\s*\}\s*$").expect("valid regex")
    });
    if !re.is_match(line) {
        return None;
    }
    Some(TautologyFinding {
        line: line_no,
        pattern: "empty_test_body".to_string(),
        message: "Test body is empty and cannot detect regressions".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_tautological_assertions() {
        let src = r"
            #[test]
            fn test_a() {
                assert!(true);
                assert_eq!(value, value);
            }
        ";
        let findings = detect_tautological_test_patterns(src);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().any(|f| f.pattern == "assert!(true)"));
        assert!(findings.iter().any(|f| f.pattern == "assert_eq!(x, x)"));
    }

    #[test]
    fn ignores_non_tautological_assertions() {
        let src = r"
            #[test]
            fn test_real() {
                assert_eq!(actual, expected);
            }
        ";
        let findings = detect_tautological_test_patterns(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn detects_empty_test_function() {
        let src = "fn test_empty() {}";
        let findings = detect_tautological_test_patterns(src);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].pattern, "empty_test_body");
    }
}
