---
name: runtime-contract-architect
description: Define durable service and runtime contracts, ownership boundaries, and compile-enforced dependency direction.
---

# Runtime Contract Architect

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for features that freeze or refactor the permanent architecture contracts:
- typed service interfaces
- shared DTOs for surfaces
- `WorkerRuntime` or equivalent launch/runtime contracts
- dependency-direction enforcement
- authority-boundary extraction without yet implementing all downstream planes

## Required Skills

None.

## Work Procedure

1. Read the mission artifacts relevant to the feature: `mission.md`, `validation-contract.md`, `AGENTS.md`, and the specific feature entry in `features.json`.
2. Read the current source modules that still own the authority being moved. Identify exactly which modules must stop being authorities after this feature.
3. Write or update tests and boundary checks first where practical. Prefer compile-time or structural checks that make bypasses visible.
4. Introduce the typed contracts and shared DTOs before moving callers onto them.
5. Refactor callers to consume the new contracts; do not leave duplicate parallel truth unless the feature explicitly requires a temporary migration seam.
6. Run the relevant validation commands from `.factory/services.yaml` that this feature can reasonably satisfy.
7. Manually inspect the dependency direction after edits. Confirm surfaces call contracts, not engine internals.
8. In the handoff, explicitly name what authority moved, what authority still remains elsewhere, and what follow-up features now depend on this contract.

## Example Handoff

{
  "salientSummary": "Defined the shared service contracts for conversation, workflow, persistence, and worker runtime, then moved CLI and RPC startup wiring to those contracts. Added compile-oriented boundary coverage so surfaces can no longer construct workflow internals directly.",
  "whatWasImplemented": "Created typed service interfaces and shared surface DTOs for model/session/control operations, moved startup composition to use those interfaces, and added boundary-focused tests that fail if surface modules import engine-internal constructors directly. This feature established the contract kernel needed by later persistence, workflow, and runtime-plane refactors.",
  "whatWasLeftUndone": "The contracts exist, but downstream planes still need implementation-specific cutovers in later milestones. Some legacy modules still exist behind the new interfaces and are intentionally left for follow-up features.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo fmt --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --check",
        "exitCode": 0,
        "observation": "Formatting passed after contract extraction."
      },
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Relevant contract and boundary tests passed."
      }
    ],
    "interactiveChecks": []
  },
  "tests": {
    "added": [
      {
        "file": "tests/runtime_contract_boundaries.rs",
        "cases": [
          {
            "name": "surface_modules_use_service_contracts_only",
            "verifies": "CLI/RPC/TUI modules depend on service contracts instead of engine internals."
          }
        ]
      }
    ]
  },
  "discoveredIssues": [
    {
      "severity": "medium",
      "description": "A few legacy helpers still expose constructors that later features should quarantine or delete once workflow and persistence cutovers land.",
      "suggestedFix": "Track deletion in the cutover milestone after all callers are moved."
    }
  ]
}

## When to Return to Orchestrator

- The feature requires a naming or ownership decision that would alter more than one agreed plane boundary.
- The only way forward would be to keep two co-equal authorities with no clear deletion plan.
- A required follow-up cutover belongs in a different milestone and cannot be safely bundled into the current feature.
