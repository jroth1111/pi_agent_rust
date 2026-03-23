---
name: surface-adapter-refactorer
description: Thin CLI, RPC, TUI, and SDK surfaces into adapters over shared contracts while preserving user-facing transport behavior.
---

# Surface Adapter Refactorer

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for features that refactor:
- CLI startup and non-interactive command routes
- RPC transport handlers
- TUI command/queue/interrupt adapters
- SDK surface alignment for supported transports
- approval-prompt and control-schema parity across surfaces

## Required Skills

None.

## Work Procedure

1. Read the current user-facing docs for the surface you are editing (`docs/rpc.md`, `docs/tui.md`, `docs/sdk.md`, README sections) before touching code.
2. Identify the exact public behavior that must stay stable for this feature.
3. Add or update tests first for the public behavior, especially around transport shape, route selection, queue controls, or approval correlation.
4. Refactor the surface to call shared service contracts/DTOs rather than owning business-state transitions.
5. Preserve explicit fail-closed behavior for unsupported or unavailable approval/control paths, especially in non-interactive routes.
6. Run relevant validation commands and any targeted smoke checks for the surface.
7. In the handoff, distinguish between transport behavior preserved and authority moved.

## Example Handoff

{
  "salientSummary": "Refactored the RPC transport layer to delegate session and workflow mutations through shared services while preserving the documented JSONL envelope. Added parity tests for queue control and approval correlation IDs.",
  "whatWasImplemented": "Moved RPC command handlers off direct runtime-state mutation and onto the shared control/state contracts, preserved the public request/response shape, and tightened fail-closed behavior for approval-required non-interactive flows. Also aligned the TUI queue/interrupt paths to the same typed control contract used by RPC.",
  "whatWasLeftUndone": "CLI and SDK parity work remain in separate features. This change only covered the targeted surface adapters and the shared DTOs they rely on.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Transport/parity tests covering RPC control routes passed."
      }
    ],
    "interactiveChecks": [
      {
        "action": "Exercise representative RPC or TUI control flow after refactor",
        "observed": "The surface preserved the expected public behavior while delegating through the shared service contract."
      }
    ]
  },
  "tests": {
    "added": [
      {
        "file": "tests/rpc_surface_contract.rs",
        "cases": [
          {
            "name": "rpc_handlers_delegate_to_services",
            "verifies": "RPC remains transport-only while keeping the documented envelope stable."
          },
          {
            "name": "noninteractive_approval_fails_closed",
            "verifies": "Unsupported approval collection on non-interactive routes does not fall back to interactive control flow."
          }
        ]
      }
    ]
  },
  "discoveredIssues": []
}

## When to Return to Orchestrator

- The public surface contract is ambiguous and the docs/tests do not establish which behavior is canonical.
- Preserving the surface behavior would require keeping the surface as a business-logic owner.
- Approval/queue semantics differ across surfaces in ways that need an orchestrator decision.
