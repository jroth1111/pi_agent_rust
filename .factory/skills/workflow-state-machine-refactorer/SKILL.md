---
name: workflow-state-machine-refactorer
description: Unify workflow lifecycle, launches, leases, blockers, idempotency, and runtime entry under one authoritative workflow engine.
---

# Workflow State Machine Refactorer

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for features that modify:
- workflow lifecycle ownership
- run/task/lease/blocker state
- worker launch records and `WorkerRuntime`
- handoffs
- idempotent command handling
- task-attempt/workspace binding for completion paths

## Required Skills

None.

## Work Procedure

1. Read the workflow-related mission assertions and inspect the current split authority across `orchestration`, `reliability`, `state`, `task_graph`, and RPC paths.
2. Add or update tests first for the precise state-transition behavior being unified.
3. Move one authority at a time. Avoid partial refactors that leave lifecycle truth split between old and new models without a clearly bounded seam.
4. Ensure launches are durable before runtime starts.
5. Ensure leases/fences, handoffs, and duplicate command replays are handled as persisted workflow concerns.
6. Ensure no launch path bypasses `WorkerRuntime`.
7. Ensure task-attempt/workspace bindings are durable anywhere verification or completion depends on them.
8. Run the relevant validation commands and targeted workflow tests.
9. In the handoff, call out any remaining old lifecycle owners or migration seams.

## Example Handoff

{
  "salientSummary": "Unified workflow launch and handoff state under the new workflow engine and removed the last direct default-session launch shortcut. Added idempotency coverage so duplicate dispatch and submit commands no longer double-advance state.",
  "whatWasImplemented": "Moved launch, lease, handoff, and replay-sensitive transitions into the authoritative workflow store/journal, introduced a complete pre-execution launch record, and routed all execution through WorkerRuntime. Also added task-attempt/workspace bindings so completion depends on the same durable attempt and snapshot chain across launch, verify, and submit.",
  "whatWasLeftUndone": "Verification gating semantics are handled by a separate feature; this work established the workflow-side lifecycle needed for those decisions to be authoritative.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Workflow state-machine, lease, and idempotency tests passed."
      }
    ],
    "interactiveChecks": []
  },
  "tests": {
    "added": [
      {
        "file": "tests/workflow_state_machine_unified.rs",
        "cases": [
          {
            "name": "duplicate_dispatch_is_idempotent",
            "verifies": "Replay/retry cannot produce duplicate state transitions."
          },
          {
            "name": "launch_record_exists_before_runtime_start",
            "verifies": "Execution cannot begin without a persisted launch envelope."
          }
        ]
      }
    ]
  },
  "discoveredIssues": []
}

## When to Return to Orchestrator

- The feature would need to keep multiple co-equal workflow owners active beyond a tightly bounded migration seam.
- Required launch or lease semantics are ambiguous relative to the validation contract.
- Verification/completion semantics need a policy decision before the workflow refactor can be finished safely.
