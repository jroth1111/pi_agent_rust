---
name: runtime-plane-engineer
description: Extract shared runtime planes for inference, context, execution, workspace, identity, capability, and admission without collapsing their ownership boundaries.
---

# Runtime Plane Engineer

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for features that create or cut over shared planes:
- inference
- context/retrieval
- host execution
- workspace
- identity/secrets
- capability/approval
- resource/admission

## Required Skills

None.

## Work Procedure

1. Read the plane-specific mission assertions and identify exactly which code paths currently leak this concern outside its intended owner.
2. Add or update tests first for the external behavior and durable records that prove the plane owns the concern.
3. Extract the plane boundary carefully. Do not collapse multiple planes into a single generic policy blob.
4. Move callers onto the plane, leaving only a clearly bounded seam if a temporary transition is unavoidable.
5. Add durable receipts/journal state where the validation contract requires it.
6. Run relevant validation commands and targeted tests.
7. In the handoff, name every caller class moved onto the plane and any caller class still outstanding.

## Example Handoff

{
  "salientSummary": "Introduced the host execution plane and moved built-ins, workers, and verifier command execution onto a shared execution boundary with common receipts and mediation semantics.",
  "whatWasImplemented": "Created a shared host execution entrypoint with durable execution receipts, centralized timeout/cancellation/mediation enforcement, and integrated admission checks. Migrated built-in tool execution and workflow verifier execution onto that boundary so execution semantics are no longer caller-specific.",
  "whatWasLeftUndone": "Extension runtime routing onto the same boundary remains for a later cutover feature, though the execution plane contract is now available for that work.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Shared plane tests for execution receipts and caller-class parity passed."
      }
    ],
    "interactiveChecks": []
  },
  "tests": {
    "added": [
      {
        "file": "tests/host_execution_plane.rs",
        "cases": [
          {
            "name": "equivalent_callers_share_execution_semantics",
            "verifies": "Built-ins, workers, and verifiers receive equivalent execution enforcement and receipts."
          }
        ]
      }
    ]
  },
  "discoveredIssues": [
    {
      "severity": "medium",
      "description": "One extension runtime call path still bypasses the new execution plane and should be cut over in the extension-runtime milestone.",
      "suggestedFix": "Track the remaining caller migration in the cutover milestone."
    }
  ]
}

## When to Return to Orchestrator

- The feature requires collapsing two or more intended planes into one owner to make progress.
- A boundary decision between planes is ambiguous and would materially change the approved architecture.
- Environment blockers prevent running the relevant plane-level tests and the feature cannot be trusted without them.
