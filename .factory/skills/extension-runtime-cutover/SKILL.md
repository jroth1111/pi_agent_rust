---
name: extension-runtime-cutover
description: Place providers, extension runtime, instruction catalog, and transitional default paths into their final owning architecture and remove compatibility-first behavior.
---

# Extension Runtime Cutover

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for final-placement and cutover features involving:
- provider adapter subordination
- extension runtime selection and compatibility runtime gating
- instruction catalog separation
- default route deletion/quarantine
- restore-time fail-closed behavior for runtime incompatibility

## Required Skills

None.

## Performance Discipline

- Keep assistant output terse. Do not narrate before every tool call or emit long reasoning summaries.
- Keep the todo list short, at most 5 items, and update it only when task state changes materially.
- On resumed or interrupted sessions, inspect the current git diff, touched files, and latest failing command output before making new edits. Continue from the existing state; do not restate the whole architecture in prose.
- Read only the cutover assertions and the exact transitional paths being removed or quarantined.
- Use manifest-declared cargo commands or their exact embedded absolute invocations only.
- Prefer targeted extension/runtime tests before broader cargo runs.
- Remove one default path or compatibility seam at a time; avoid broad unrelated cleanup during cutover.
- After two failed edit attempts caused by stale context or mismatched patches, re-read the exact file and apply one minimal fix. If still blocked, hand off instead of looping.
- After two failed validation attempts with the same root cause, stop retrying variants and return a structured handoff with the blocker.
- After any BYOK/provider `429`, `TimeoutError`, or Fair Use restriction message in-session, stop and hand off instead of continuing with more planning or retries.
- Once one authority move and one focused validation pass are complete, either apply one minimal fix or hand off. Do not start a second broad rewrite in the same session.

## Work Procedure

1. Read the cutover assertions and identify the exact transitional or compatibility-first behavior that must stop being authoritative.
2. Add or update tests first for startup selection, restore behavior, fail-closed cases, and dependency boundaries.
3. Make runtime selection explicit and observable.
4. Ensure compatibility runtime paths are opt-in only for migration/test/import cases.
5. Ensure declarative instruction assets remain separate from executable extension authority.
6. Delete or quarantine transitional defaults rather than leaving them silently reachable.
7. Run relevant validation commands and targeted extension/runtime tests.
8. In the handoff, name what transitional path was removed/quarantined and what remains intentionally for migration-only use.

## Example Handoff

{
  "salientSummary": "Removed compatibility-first extension startup and made production runtime selection explicit at the runtime-to-host seam. Restore-time incompatibility now fails closed with a durable runtime-resolution record.",
  "whatWasImplemented": "Moved production extension runtime selection behind a single explicit selector, gated compatibility execution to migration/test-only entrypoints, and separated instruction-catalog loading from executable extension authority. Also added dependency-boundary checks so provider adapters and runtime support code cannot act like peer control planes anymore.",
  "whatWasLeftUndone": "A small migration-only compatibility utility remains intentionally reachable behind explicit flags for import/testing; it is no longer on the default route.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Extension runtime selection and fail-closed restore tests passed."
      }
    ],
    "interactiveChecks": []
  },
  "tests": {
    "added": [
      {
        "file": "tests/extension_runtime_cutover.rs",
        "cases": [
          {
            "name": "production_runtime_is_not_compatibility_first",
            "verifies": "Ordinary startup does not silently choose the compatibility runtime."
          },
          {
            "name": "restore_incompatible_runtime_fails_closed",
            "verifies": "Restore-time runtime mismatch produces a durable fail-closed result."
          }
        ]
      }
    ]
  },
  "discoveredIssues": []
}

## When to Return to Orchestrator

- The only way to complete the feature would keep compatibility-first behavior on the default route.
- Restore/startup semantics are ambiguous enough that multiple incompatible fail-closed behaviors seem plausible.
- A remaining migration-only path appears to still be required for default startup, which would contradict the mission boundaries.
