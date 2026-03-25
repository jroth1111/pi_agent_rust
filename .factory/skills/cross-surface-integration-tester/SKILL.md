---
name: cross-surface-integration-tester
description: Prove cross-area joins, parity, restart recovery, and fail-closed behavior after the core architecture cutovers are in place.
---

# Cross Surface Integration Tester

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for final integration leaves that prove:
- cross-surface session parity
- cross-area workflow provenance
- restart recovery
- extension end-to-end parity and fail-closed cases
- joins across workflow, approval, identity, workspace, context, runtime, execution/inference, and persistence

## Required Skills

None.

## Performance Discipline

- Keep assistant output terse. Do not narrate before every tool call or emit long reasoning summaries.
- Keep the todo list short, at most 5 items, and update it only when task state changes materially.
- Prove only the joins claimed by the feature. Do not broaden the integration scope once the representative surfaces and records are identified.
- Prefer targeted integration tests and one representative live surface check over broad exploratory runs.
- After two failed edit attempts caused by stale context or mismatched patches, re-read the exact file and apply one minimal fix. If still blocked, hand off instead of looping.
- After two failed validation attempts with the same root cause, stop retrying variants and return a structured handoff with the blocker.

## Work Procedure

1. Read the cross-area assertions claimed by the feature and list the exact records/transcripts needed to prove each join.
2. Do not introduce new core architecture in this feature unless absolutely required to make the integration path testable. Prefer integration glue and test coverage.
3. Add or update tests first for the end-to-end joins you need to prove.
4. Exercise at least the representative surfaces or caller classes named in the feature.
5. Verify durable identifiers can be joined across all required records; do not accept hand-wavy or log-only evidence.
6. Run relevant validation commands and targeted integration tests.
7. In the handoff, list the exact joins proven and any remaining gap that still lacks durable correlation.

## Example Handoff

{
  "salientSummary": "Added the final cross-surface and cross-plane integration coverage for session parity and workflow provenance. Restart recovery, approval lineage, workspace/context/runtime joins, and extension fail-closed behavior are now traceable through stable durable identifiers.",
  "whatWasImplemented": "Built integration coverage that proves equivalent surface actions land in the same authoritative session/workflow records and that worker launches can be traced from workflow command through identity, approval, admission, workspace, context, runtime, execution/inference, and verification records. Also covered extension-contributed model/provider fail-closed behavior and extension UI request parity between RPC and TUI.",
  "whatWasLeftUndone": "No major core architecture remains in this slice; any remaining failures are genuine integration gaps or environment blockers.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Cross-surface integration and provenance tests passed."
      }
    ],
    "interactiveChecks": [
      {
        "action": "Exercise one representative TUI or RPC cross-surface flow",
        "observed": "Surface-visible behavior matched the durable records produced by the new architecture."
      }
    ]
  },
  "tests": {
    "added": [
      {
        "file": "tests/runtime_control_plane_integration.rs",
        "cases": [
          {
            "name": "worker_launch_provenance_chain_is_joinable",
            "verifies": "Workflow -> identity/approval -> admission -> workspace -> context -> runtime -> verification lineage is durable and queryable."
          },
          {
            "name": "extension_contributed_provider_failures_are_fail_closed",
            "verifies": "Unavailable/incompatible extension-contributed providers do not silently substitute or partially execute."
          }
        ]
      }
    ]
  },
  "discoveredIssues": []
}

## When to Return to Orchestrator

- The feature uncovers a missing durable identifier or missing journal hop that requires new core architecture in a previous milestone.
- Cross-surface parity depends on undocumented behavior and the mission artifacts do not say which behavior is canonical.
- Environment blockers prevent the required end-to-end validation from running, and the missing evidence cannot be reasonably inferred.
