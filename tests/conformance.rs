//! Reliability quality-gate conformance harness.
//!
//! This target exists to enforce anti-tautology quality checks in fixture-driven
//! conformance flows.

#[path = "conformance/mod.rs"]
mod conformance;

#[path = "conformance/fixture_runner.rs"]
mod fixture_runner;

use conformance::{Expected, FixtureFile, TestCase};

fn empty_case(name: &str) -> TestCase {
    TestCase {
        name: name.to_string(),
        description: String::new(),
        setup: Vec::new(),
        input: serde_json::json!({}),
        expected: Expected::default(),
        expect_error: false,
        error_contains: None,
        tags: Vec::new(),
        quality_probe_source: None,
    }
}

#[test]
fn known_tautological_fixture_fails_quality_gate() {
    asupersync::test_utils::run_test(|| async {
        let mut case = empty_case("tautology");
        case.expected.content_contains = vec!["ok".to_string()];
        case.quality_probe_source = Some("assert!(true);".to_string());

        let fixture = FixtureFile {
            version: "1.0".to_string(),
            tool: "read".to_string(),
            description: "tautology gate".to_string(),
            cases: vec![case],
        };

        let results = fixture_runner::run_fixture_tests(&fixture).await;
        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        let msg = results[0].message.clone().unwrap_or_default();
        assert!(msg.contains("Quality gate rejected case"));
        assert!(msg.contains("tautological assertions"));
    });
}

#[test]
fn realistic_fixture_passes_quality_gate_and_executes() {
    asupersync::test_utils::run_test(|| async {
        let mut case = empty_case("cli_help");
        case.input = serde_json::json!({ "args": ["--help"] });
        case.expected.content_contains = vec!["Usage".to_string()];

        let fixture = FixtureFile {
            version: "1.0".to_string(),
            tool: "cli_flags".to_string(),
            description: "realistic gate".to_string(),
            cases: vec![case],
        };

        let results = fixture_runner::run_fixture_tests(&fixture).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "{:?}", results[0].message);
    });
}
