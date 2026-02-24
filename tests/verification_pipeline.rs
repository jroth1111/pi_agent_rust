//! Integration tests for the verification pipeline.
//!
//! Exercises the end-to-end flow: evidence collection -> test analysis
//! -> quality scoring -> guard decision -> attestation token.

use pi::verification::{
    AttestationManager, AttestationPayload, AttestationToken, CompletionGate, Evidence,
    EvidenceKind, EvidenceStore, TOKEN_VERSION, TestAnalyzer, TestGuardResult, TestPerspective,
    TestQualityIssue, TestResult, VerificationOutcome, VerifyPlanBuilder, calculate_quality_score,
};

/// End-to-end: evidence collection -> analysis -> quality score -> gate decision.
#[test]
fn evidence_analysis_quality_gate_pipeline() {
    // 1. Collect evidence
    let store = EvidenceStore::new();

    let file_evidence = Evidence::from_file("src/auth/mod.rs", "pub fn authenticate() { ... }");
    let test_evidence = Evidence::from_test("test_authenticate", "passed");
    let diff_evidence = Evidence::from_diff("HEAD~1", "HEAD", "+pub fn authenticate() { ... }");

    store.add(file_evidence).unwrap();
    store.add(test_evidence).unwrap();
    store.add(diff_evidence).unwrap();

    assert_eq!(store.len(), 3);
    assert!(store.has_kind("file_content"));
    assert!(store.has_kind("test_result"));
    assert!(store.has_kind("diff"));

    // 2. Analyze test quality - good tests should score well
    let analyzer = TestAnalyzer::new();
    let good_test_source = r#"
#[test]
fn test_authenticate_valid_token() {
    let result = authenticate("valid-token");
    assert!(result.is_ok());
    assert_eq!(result.unwrap().user_id, "user-1");
}

#[test]
fn test_authenticate_invalid_token() {
    let result = authenticate("bad-token");
    assert!(result.is_err());
}
"#;

    let issues = analyzer.analyze(good_test_source);
    assert!(
        issues.is_empty(),
        "Good tests should have no quality issues, got: {issues:?}"
    );

    // 3. Quality score for no issues should be perfect
    let issue_refs: Vec<&TestQualityIssue> = issues.iter().collect();
    let quality_score = calculate_quality_score(&issue_refs, 100);
    assert!((quality_score - 1.0).abs() < f32::EPSILON);

    // 4. Completion gate with evidence requirements
    let gate = CompletionGate::strict();
    let all_evidence = store.all();
    gate.check(0, true, &all_evidence).unwrap();
}

/// Verify pipeline detects poor test quality.
#[test]
fn pipeline_detects_poor_quality_tests() {
    let analyzer = TestAnalyzer::new();
    let bad_test_source = r"
#[test]
fn test_trivial() {
    assert!(true);
}

#[test]
fn test_empty() {
}
";

    let issues = analyzer.analyze(bad_test_source);
    assert!(!issues.is_empty());

    let has_trivial = issues
        .iter()
        .any(|i| matches!(i, TestQualityIssue::TrivialAssertion { .. }));
    let has_empty = issues
        .iter()
        .any(|i| matches!(i, TestQualityIssue::EmptyTest { .. }));

    assert!(
        has_trivial || has_empty,
        "Should detect at least one quality issue"
    );

    // Quality score should be penalized
    let issue_refs: Vec<&TestQualityIssue> = issues.iter().collect();
    let score = calculate_quality_score(&issue_refs, 50);
    assert!(score < 1.0, "Score should be less than perfect: {score}");
}

/// Attestation token round-trip: sign, encode, decode, verify.
#[test]
fn attestation_token_roundtrip() {
    let key: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

    let payload = AttestationPayload::passed(
        "task-integration-test".to_string(),
        vec!["evidence-1".to_string(), "evidence-2".to_string()],
        42,
    );

    let token = AttestationToken::sign(payload, &key);
    assert!(token.is_passed());
    assert!(token.verify(&key));

    // Encode and decode
    let encoded = token.encode();
    assert!(encoded.starts_with(TOKEN_VERSION));

    let decoded = AttestationToken::decode_and_verify(&encoded, &key).unwrap();
    assert_eq!(decoded.task_id(), "task-integration-test");
    assert_eq!(decoded.fence(), 42);
    assert!(decoded.is_passed());

    // Wrong key fails verification
    let wrong_key: [u8; 16] = [0; 16];
    assert!(AttestationToken::decode_and_verify(&encoded, &wrong_key).is_err());
}

/// Attestation manager tracks monotonic fences.
#[test]
fn attestation_manager_fence_monotonicity() {
    let mut manager = AttestationManager::from_seed("test-seed");

    let t1 = manager.attest_passed("task-1", vec!["e1".to_string()]);
    let t2 = manager.attest_passed("task-2", vec!["e2".to_string()]);
    let t3 = manager.attest_failed("task-3", vec![]);

    assert_eq!(t1.fence(), 1);
    assert_eq!(t2.fence(), 2);
    assert_eq!(t3.fence(), 3);

    // All tokens verifiable by the same manager
    assert!(manager.verify(&t1));
    assert!(manager.verify(&t2));
    assert!(manager.verify(&t3));

    // Different seed produces incompatible tokens
    let other_manager = AttestationManager::from_seed("other-seed");
    assert!(!other_manager.verify(&t1));
}

/// Guard with insufficient evidence fails.
#[test]
fn guard_requires_sufficient_evidence() {
    let gate = CompletionGate::strict();

    // No evidence at all
    let err = gate.check(0, true, &[]).unwrap_err();
    assert!(err.to_string().contains("missing required evidence"));

    // Only file evidence (missing test)
    let file_evidence = Evidence::from_file("test.rs", "content");
    let err = gate
        .check(0, true, std::slice::from_ref(&file_evidence))
        .unwrap_err();
    assert!(err.to_string().contains("missing required evidence"));

    // File + test evidence passes
    let test_evidence = Evidence::from_test("test_foo", "passed");
    gate.check(0, true, &[file_evidence, test_evidence])
        .unwrap();
}

/// Completion gate blocks on open TODOs.
#[test]
fn completion_gate_blocks_open_todos() {
    let gate = CompletionGate::todos_only();

    let err = gate.check(3, false, &[]).unwrap_err();
    assert!(err.to_string().contains("3 open TODO"));

    // Zero open todos passes
    gate.check(0, false, &[]).unwrap();
}

/// Verification plan with perspectives.
#[test]
fn verify_plan_perspective_coverage() {
    let plan = VerifyPlanBuilder::new()
        .command("cargo test")
        .timeout_secs(120)
        .require_perspective(TestPerspective::HappyPath)
        .require_perspective(TestPerspective::Negative)
        .require_perspective(TestPerspective::Boundary)
        .build()
        .unwrap();

    // Missing boundary perspective
    assert!(!plan.validate_perspectives(&[TestPerspective::HappyPath, TestPerspective::Negative,]));

    // All perspectives covered
    assert!(plan.validate_perspectives(&[
        TestPerspective::HappyPath,
        TestPerspective::Negative,
        TestPerspective::Boundary,
    ]));

    assert!(plan.is_success_code(0));
    assert!(!plan.is_success_code(1));
}

/// Evidence store keyed retrieval.
#[test]
fn evidence_store_retrieval_patterns() {
    let store = EvidenceStore::new();

    store.add(Evidence::from_file("a.rs", "fn a() {}")).unwrap();
    store.add(Evidence::from_file("b.rs", "fn b() {}")).unwrap();
    store
        .add(Evidence::from_command("cargo test", "ok"))
        .unwrap();
    store.add(Evidence::from_test("test_a", "passed")).unwrap();
    store
        .add(Evidence::from_diff("v1", "v2", "+new_line"))
        .unwrap();

    assert_eq!(store.len(), 5);
    assert_eq!(store.get_by_kind("file_content").len(), 2);
    assert_eq!(store.get_by_kind("command_output").len(), 1);
    assert_eq!(store.get_by_kind("test_result").len(), 1);
    assert_eq!(store.get_by_kind("diff").len(), 1);
    assert!(store.get_by_kind("nonexistent").is_empty());

    // Clear and verify
    store.clear().unwrap();
    assert!(store.is_empty());
}

/// Verification outcome conversion from `TestGuardResult`.
#[test]
fn test_guard_result_to_verification_outcome() {
    let passed = TestGuardResult::Passed {
        test_result: TestResult::success("All tests passed".to_string(), 1234),
        evidence_ids: vec!["e1".to_string(), "e2".to_string()],
        state_diff: None,
    };
    assert!(passed.allows_completion());

    let outcome: VerificationOutcome = passed.into();
    assert!(outcome.passed);
    assert_eq!(outcome.evidence_collected.len(), 2);

    let failed = TestGuardResult::Failed {
        test_result: TestResult::failure(1, "assertion failed".to_string(), 500),
        evidence_ids: vec!["e1".to_string()],
        state_diff: None,
    };
    assert!(!failed.allows_completion());

    let outcome: VerificationOutcome = failed.into();
    assert!(!outcome.passed);

    // Skipped and Overridden allow completion
    let skipped = TestGuardResult::Skipped {
        reason: "not required".to_string(),
    };
    assert!(skipped.allows_completion());

    assert!(TestGuardResult::Overridden.allows_completion());
}

/// Evidence kind matching for completion gates.
#[test]
fn evidence_kind_matching() {
    let file_kind = EvidenceKind::FileContent {
        path: "src/main.rs".to_string(),
    };
    assert!(file_kind.matches_requirement("file"));
    assert!(file_kind.matches_requirement("file_content"));
    assert!(file_kind.matches_requirement("src/main.rs"));
    assert!(!file_kind.matches_requirement("test"));

    let cmd_kind = EvidenceKind::CommandOutput {
        command: "cargo test".to_string(),
    };
    assert!(cmd_kind.matches_requirement("command"));
    assert!(cmd_kind.matches_requirement("cargo test"));
    assert!(!cmd_kind.matches_requirement("file"));

    let test_kind = EvidenceKind::TestResult {
        test_name: "test_foo".to_string(),
    };
    assert!(test_kind.matches_requirement("test"));
    assert!(test_kind.matches_requirement("test_foo"));

    let diff_kind = EvidenceKind::Diff {
        from: "a".to_string(),
        to: "b".to_string(),
    };
    assert!(diff_kind.matches_requirement("diff"));
    assert!(diff_kind.matches_requirement("a -> b"));
}
