---
name: verification-policy-enforcer
description: Make verification, evidence, scope, overrides, and completion gating durable, explicit, and workflow-owned.
---

# Verification Policy Enforcer

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for features that change:
- verification evidence and acceptance mappings
- verification scope semantics
- completion gating
- override approval/supersession
- verification history durability
- workflow-visible verification migration/cutover

## Required Skills

None.

## Performance Discipline

- Keep assistant output terse. Do not narrate before every tool call or emit long reasoning summaries.
- Keep the todo list short, at most 5 items, and update it only when task state changes materially.
- On resumed or interrupted sessions, inspect the current git diff, touched files, and latest failing command output before making new edits. Continue from the existing state; do not restate the whole architecture in prose.
- Read only the verification assertions, completion paths, and workflow modules directly relevant to the feature.
- Use manifest-declared cargo commands or their exact embedded absolute invocations only.
- Prefer targeted verification and workflow tests before broader cargo runs.
- Move one authoritative completion or override path at a time; avoid broad policy rewrites in one run.
- After two failed edit attempts caused by stale context or mismatched patches, re-read the exact file and apply one minimal fix. If still blocked, hand off instead of looping.
- After two failed validation attempts with the same root cause, stop retrying variants and return a structured handoff with the blocker.
- After any BYOK/provider `429`, `TimeoutError`, or Fair Use restriction message in-session, stop and hand off instead of continuing with more planning or retries.
- Once one authority move and one focused validation pass are complete, either apply one minimal fix or hand off. Do not start a second broad rewrite in the same session.

## Work Procedure

1. Read the verification assertions, current verification modules, and any workflow-side completion paths before editing.
2. Add or update tests first for the precise gating semantics you are changing.
3. Make verification decisions durable and bind them to the correct attempt/workspace/input scope.
4. Ensure acceptance mappings and evidence references are explicit and inspectable.
5. Ensure overrides require durable approval provenance and explicit scope/supersession semantics.
6. Ensure deferred/failed/retried verification history remains queryable after restart.
7. Run relevant validation commands and targeted verification/workflow tests.
8. In the handoff, explicitly state which completion path is now authoritative and what old path, if any, still exists.

## Example Handoff

{
  "salientSummary": "Moved completion gating fully onto durable workflow-owned verification decisions. Added scoped override provenance and durable failure history so restart/retry cannot hide why a task remained blocked.",
  "whatWasImplemented": "Bound verification decisions to task attempts, workspace snapshots, and relevant inputs; persisted acceptance mappings and evidence refs; and made Task/Run/Handoff/SessionClose scope handling explicit. Overrides now require durable approval records with explicit scope and supersession lineage, and verification deferrals/failures remain queryable after restart.",
  "whatWasLeftUndone": "Cross-surface completion/reporting parity is handled later in the integration milestone; this feature made the workflow-side verification truth authoritative.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Verification gating and override tests passed."
      }
    ],
    "interactiveChecks": []
  },
  "tests": {
    "added": [
      {
        "file": "tests/verification_workflow_gating.rs",
        "cases": [
          {
            "name": "submit_without_verification_is_rejected",
            "verifies": "Workflow success cannot occur without a durable verification decision or approved override."
          },
          {
            "name": "override_requires_scope_and_supersession_records",
            "verifies": "Overrides are explicit, scoped, and durably correlated."
          }
        ]
      }
    ]
  },
  "discoveredIssues": []
}

## When to Return to Orchestrator

- Completion/override policy needs a business or security decision not settled by mission.md or the validation contract.
- Evidence/acceptance semantics are ambiguous enough that multiple plausible implementations would satisfy tests differently.
- Environment blockers prevent running the verification/workflow tests needed to prove the feature. 
