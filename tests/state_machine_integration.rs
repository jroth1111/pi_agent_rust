//! Integration tests for the state machine lifecycle.
//!
//! Exercises the full composition: `RuntimeState` transitions, `TaskEventLog`,
//! `CompletionGates`, `DiscoveryTracker`, `TaskLedger`, and `HandoffContext`.

use pi::state::{
    CompletionGates, DiscoveryPriority, DiscoveryTracker, EvidenceGate, GateBuilder, GateResult,
    HandoffContext, RuntimeState, ScopeBudget, TaskEventKind, TaskEventLog, TaskLedger, TaskResult,
};
use std::path::PathBuf;

/// Full lifecycle: Ready -> Leased -> Verifying -> Terminal(Success)
/// with event logging at each step, verifying state reconstruction.
#[test]
fn full_lifecycle_claim_verify_complete() {
    let mut log = TaskEventLog::new();

    // 1. Created
    log.append(TaskEventKind::Created, Some("agent-1".to_string()));
    assert!(log.reconstruct_state().is_claimable());

    // 2. Claimed
    log.append(
        TaskEventKind::Claimed { fence: 1 },
        Some("agent-1".to_string()),
    );
    let state = log.reconstruct_state();
    assert!(state.is_leased());
    assert_eq!(state.lease_holder(), Some("agent-1"));

    // 3. Todo added during work
    log.append(
        TaskEventKind::TodoAdded {
            todo_id: "t1".to_string(),
            content: "Write tests".to_string(),
        },
        Some("agent-1".to_string()),
    );

    // 4. Todo completed
    log.append(
        TaskEventKind::TodoStatusChanged {
            todo_id: "t1".to_string(),
            from: "pending".to_string(),
            to: "completed".to_string(),
        },
        Some("agent-1".to_string()),
    );

    // 5. Gate registered
    log.append(
        TaskEventKind::GateRegistered {
            gate_name: "tests-pass".to_string(),
        },
        Some("agent-1".to_string()),
    );

    // 6. Gate passed
    log.append(
        TaskEventKind::GatePassed {
            gate_name: "tests-pass".to_string(),
        },
        Some("agent-1".to_string()),
    );

    // 7. Verification started
    log.append(
        TaskEventKind::VerificationStarted,
        Some("agent-1".to_string()),
    );
    assert!(matches!(log.reconstruct_state(), RuntimeState::Verifying));

    // 8. Completed
    log.append(
        TaskEventKind::Completed {
            result: TaskResult::Success,
            reason: "All acceptance criteria met".to_string(),
            evidence: vec![],
        },
        Some("agent-1".to_string()),
    );

    let final_state = log.reconstruct_state();
    assert!(final_state.is_terminal());

    // Verify event counts
    assert_eq!(log.len(), 8);
    assert_eq!(log.state_transitions().len(), 3); // Claimed, VerificationStarted, Completed
    assert_eq!(log.terminal_events().len(), 1);
    assert_eq!(log.by_agent("agent-1").len(), 8);
}

/// Event log integrity after multi-step transitions.
#[test]
fn event_log_integrity_after_release_and_reclaim() {
    let mut log = TaskEventLog::new();

    // Agent 1 claims and releases
    log.append(TaskEventKind::Created, None);
    log.append(
        TaskEventKind::Claimed { fence: 1 },
        Some("agent-1".to_string()),
    );
    log.append(
        TaskEventKind::Released { fence: 1 },
        Some("agent-1".to_string()),
    );

    // Task is back to Ready
    assert!(log.reconstruct_state().is_claimable());

    // Agent 2 claims
    log.append(
        TaskEventKind::Claimed { fence: 2 },
        Some("agent-2".to_string()),
    );
    let state = log.reconstruct_state();
    assert_eq!(state.lease_holder(), Some("agent-2"));

    // Agent 2 completes
    log.append(
        TaskEventKind::Completed {
            result: TaskResult::Success,
            reason: "Done by agent-2".to_string(),
            evidence: vec![],
        },
        Some("agent-2".to_string()),
    );

    assert!(log.reconstruct_state().is_terminal());
    assert_eq!(log.total_count(), 5);

    // Verify serialization roundtrip preserves everything
    let bytes = log.serialize().unwrap();
    let restored = TaskEventLog::deserialize(&bytes).unwrap();
    assert_eq!(restored.len(), 5);
    assert!(restored.reconstruct_state().is_terminal());
}

/// Discovery tracker feeding into ledger: track scope during work.
#[test]
fn discovery_tracker_scope_management() {
    let budget = ScopeBudget::new(5, 2);
    let mut tracker = DiscoveryTracker::with_budget(budget);

    // Discover items during work
    let d1 = tracker
        .discover(
            "Missing null check",
            "task-1",
            DiscoveryPriority::MustAddress,
        )
        .unwrap()
        .id
        .clone();
    tracker
        .discover(
            "Refactor opportunity",
            "task-1",
            DiscoveryPriority::NiceToHave,
        )
        .unwrap();
    tracker
        .discover(
            "API deprecation",
            "task-1",
            DiscoveryPriority::ShouldAddress,
        )
        .unwrap();

    // Summary reflects unaddressed state
    let summary = tracker.summary();
    assert_eq!(summary.total, 3);
    assert_eq!(summary.unaddressed, 3);
    assert!(summary.has_blocking());

    // Address the blocking item
    tracker.address(&d1);
    let summary = tracker.summary();
    assert_eq!(summary.must_address_unaddressed, 0);
    assert!(!summary.has_blocking());

    // Unaddressed items sorted by priority
    let unaddressed = tracker.unaddressed();
    assert_eq!(unaddressed.len(), 2);
    // ShouldAddress (weight 2) before NiceToHave (weight 1)
    assert_eq!(unaddressed[0].priority, DiscoveryPriority::ShouldAddress);
    assert_eq!(unaddressed[1].priority, DiscoveryPriority::NiceToHave);
}

/// Handoff context generation from terminal state.
#[test]
fn handoff_context_from_completed_task() {
    use pi::state::{ArtifactRef, TodoItem, TodoStatus};

    let mut ctx = HandoffContext::new_with_id(
        "Implement auth refactor".to_string(),
        PathBuf::from("/project/src"),
    );

    // Add completed and open todos
    ctx.todos.push(TodoItem {
        id: "todo-1".to_string(),
        content: "Extract auth module".to_string(),
        status: TodoStatus::Completed,
        created_at: 1000,
        updated_at: 2000,
    });
    ctx.todos.push(TodoItem {
        id: "todo-2".to_string(),
        content: "Write integration tests".to_string(),
        status: TodoStatus::Pending,
        created_at: 1000,
        updated_at: 1000,
    });

    // Record work done
    ctx.set_last_action("Extracted auth into separate module".to_string(), None);
    ctx.add_dead_end("in-memory session store", "race condition under load");
    ctx.add_artifact(ArtifactRef::file(
        PathBuf::from("/project/src/auth/mod.rs"),
        "New auth module".to_string(),
        2000,
    ));
    ctx.suggested_next_step = "Write integration tests for the new auth module".to_string();
    ctx.context_percentage = 45.0;

    // Verify handoff integrity
    assert!(ctx.can_restore());
    assert!(ctx.has_dead_ends());
    assert!(!ctx.has_error());
    assert_eq!(ctx.open_todo_count(), 1);
    assert!(!ctx.is_complete());

    // Serialization roundtrip
    let json = ctx.to_json_string().unwrap();
    let restored = HandoffContext::from_json_string(&json).unwrap();
    assert_eq!(restored.todos.len(), 2);
    assert_eq!(restored.dead_ends.len(), 1);
    assert_eq!(restored.artifacts.len(), 1);
    assert_eq!(restored.open_todo_count(), 1);

    // Summary captures key info
    let summary = restored.summary();
    assert_eq!(summary.open_todos, 1);
    assert_eq!(summary.total_todos, 2);
    assert!(summary.can_restore);
    assert_eq!(summary.dead_ends, 1);
}

/// Fence token race: claiming with stale fence fails.
#[test]
fn fence_token_prevents_stale_operations() {
    let state = RuntimeState::Ready;
    let claimed = state.claim("agent-1".to_string(), 1000).unwrap();

    // Correct fence works
    assert!(claimed.start_verification(1).is_ok());

    // Stale fence fails
    assert!(claimed.release(999).is_err());
    assert!(claimed.start_verification(0).is_err());

    // Double claim fails
    assert!(claimed.claim("agent-2".to_string(), 2000).is_err());
}

/// Completion gates: required gates block, recommended gates don't.
#[test]
fn completion_gates_mixed_severity() {
    let mut gates = CompletionGates::new();

    gates.register(
        GateBuilder::new("tests-pass")
            .description("All tests must pass")
            .required()
            .build_fn(|| Ok(GateResult::passed("42 tests passed"))),
    );

    gates.register(
        GateBuilder::new("coverage")
            .description("Coverage should be >80%")
            .recommended()
            .build_fn(|| Ok(GateResult::failed("Coverage is 75%"))),
    );

    gates.register(
        GateBuilder::new("lint")
            .description("Lint check info")
            .informational()
            .build_fn(|| Ok(GateResult::passed("No lint issues"))),
    );

    // run_all succeeds because only recommended gate failed
    let summary = gates.run_all().unwrap();
    assert!(summary.is_success());
    assert_eq!(summary.required_passed, 1);
    assert_eq!(summary.recommended_failed, 1);
    assert_eq!(summary.informational_passed, 1);
    assert_eq!(summary.total_run(), 3);
}

/// Ledger validates completion: open todos block.
#[test]
fn ledger_completion_validation() {
    let mut ledger = TaskLedger::new();
    let t1 = ledger.add_todo("Task 1".to_string(), 1000);
    let t2 = ledger.add_todo("Task 2".to_string(), 1001);

    // Cannot complete with open todos
    assert!(ledger.validate_for_completion().is_err());

    // Complete one
    ledger.complete_todo(&t1, 2000);
    assert!(ledger.validate_for_completion().is_err());

    // Complete both
    ledger.complete_todo(&t2, 2001);
    assert!(ledger.validate_for_completion().is_ok());
    assert!(ledger.is_fully_complete());
}

/// Evidence gate integration with state evidence.
#[test]
fn evidence_gate_with_state_evidence() {
    use pi::state::Evidence;

    let gate = EvidenceGate::required().with_min_evidence(2);

    // Insufficient evidence
    let single = vec![Evidence::TestOutput {
        passed: 5,
        failed: 0,
        output: "ok".to_string(),
    }];
    assert!(gate.check_evidence(&single).is_fail());

    // Sufficient evidence
    let both = vec![
        Evidence::TestOutput {
            passed: 5,
            failed: 0,
            output: "ok".to_string(),
        },
        Evidence::GitDiff {
            files_changed: 3,
            diff: "changes".to_string(),
        },
    ];
    assert!(gate.check_evidence(&both).is_pass());

    // Summary lists kinds
    let summary = gate.evidence_summary(&both);
    assert!(summary.contains("2 evidence item(s)"));
}

/// Serialization roundtrip for ledger with state.
#[test]
fn ledger_serialization_preserves_state() {
    let mut ledger = TaskLedger::new();
    let t1 = ledger.add_todo("Implement feature".to_string(), 1000);
    let t2 = ledger.add_todo("Write tests".to_string(), 1001);
    ledger.complete_todo(&t1, 2000);
    ledger.record_success("Built successfully".to_string(), 1500);
    ledger.record_failure(
        "Test failed".to_string(),
        "assertion error".to_string(),
        1600,
    );

    let json = ledger.to_json_string().unwrap();
    let restored = TaskLedger::from_json_string(&json).unwrap();

    assert_eq!(restored.todo_count(), 2);
    assert_eq!(restored.completed_count(), 1);
    assert_eq!(restored.attempt_count(), 2);
    assert!(restored.get_todo(&t1).is_some());
    assert!(restored.get_todo(&t2).is_some());
}
