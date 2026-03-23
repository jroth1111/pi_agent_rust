---
name: session-persistence-migrator
description: Replace hybrid session truth with one authoritative event store, durable continuity state, and safe migration/cutover behavior.
---

# Session Persistence Migrator

NOTE: Startup and cleanup are handled by `worker-base`. This skill defines the WORK PROCEDURE.

## When to Use This Skill

Use for features that change session authority, migration, integrity, compaction continuity, hydration behavior, autosave durability, or projection-backed reads.

## Required Skills

None.

## Work Procedure

1. Read the mission artifacts plus current session docs and code before editing (`docs/session.md`, `docs/tree.md`, relevant migration docs, `src/session*`).
2. Identify the current authoritative and non-authoritative paths. Be explicit about what the feature is removing, demoting, or preserving as migration-only.
3. Add or update tests first. Focus on branch continuity, compaction continuity, restart behavior, migration rollback, and projection rebuild behavior.
4. Implement the authoritative store or migration change carefully. Never leave silent data-loss behavior or ambiguous rollback state.
5. Ensure typed continuity state exists for branch/compaction/skill continuity where required.
6. Ensure projections are derived-only and rebuildable.
7. Run validation commands and any targeted session tests relevant to the change.
8. In the handoff, explicitly state whether any legacy path remains live and why.

## Example Handoff

{
  "salientSummary": "Completed the session-authority cutover by routing create/open/save/resume through the new event store and demoting JSONL/V2 runtime reads to migration/import paths. Added migration rollback coverage and typed continuity records for compaction and skill state.",
  "whatWasImplemented": "Reworked session persistence so one authoritative event store owns writes and current truth, moved read paths onto rebuildable projections, and added durable migration/cutover records with surviving rollback evidence. The feature also added typed continuity state so branch, compaction, and skill context survive save/resume without depending on ad hoc prompt reconstruction.",
  "whatWasLeftUndone": "Some offline import/export helpers still exist intentionally for migration support and are scheduled for later cutover cleanup.",
  "verification": {
    "commandsRun": [
      {
        "command": "/Users/gwizz/.cargo/bin/cargo test --manifest-path /Users/gwizz/CascadeProjects/pi_agent_rust/Cargo.toml --all-targets -- --test-threads=2",
        "exitCode": 0,
        "observation": "Session continuity and migration tests passed."
      }
    ],
    "interactiveChecks": []
  },
  "tests": {
    "added": [
      {
        "file": "tests/session_authority_cutover.rs",
        "cases": [
          {
            "name": "resume_uses_authoritative_store",
            "verifies": "Resume/open no longer depend on co-equal legacy truth."
          },
          {
            "name": "failed_migration_rolls_back_cleanly",
            "verifies": "Partial migration cannot become authoritative and rollback evidence survives."
          }
        ]
      }
    ]
  },
  "discoveredIssues": [
    {
      "severity": "medium",
      "description": "An old import/export helper still shares code with a legacy path that should be simplified once cutover cleanup begins.",
      "suggestedFix": "Track cleanup under the cutover milestone after all session migrations are stabilized."
    }
  ]
}

## When to Return to Orchestrator

- The only way to proceed would risk data loss, ambiguous rollback, or dual-authority writes with no safe migration plan.
- The feature requires changing the accepted session continuity semantics beyond what the validation contract states.
- Environment/toolchain issues prevent running the necessary persistence and migration verification steps.
