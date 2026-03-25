# Validation

Mission-specific validation notes and invariants.

**What belongs here:** Required gates, validation limitations, assertion coverage rules, evidence expectations.

---

## Required automated gates

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`

Use the commands in `.factory/services.yaml` as the canonical way to invoke these gates. Do not rely on bare `cargo` being on `PATH`, and do not assume `.factory/init.sh` exports persist across later shell commands.

## Evidence expectations

This architecture mission requires durable evidence, not only passing commands. Workers should prefer artifacts and records that can be joined across:
- workflow
- approval
- workspace
- context
- runtime selection
- execution/inference
- verification
- persistence

## Coverage rule

A feature that claims assertions in `fulfills` must make those assertions fully testable, not merely contribute groundwork.

## Current execution limitation

Environment/toolchain readiness is not yet fully repaired. If a worker cannot execute the approved gates because of external environment state, it must return to the orchestrator with exact blocker details instead of silently downgrading verification.

## Runtime Hygiene

- Prefer the narrowest meaningful validation first for the touched boundary before escalating to the full gate set.
- If a targeted check already proves the same external blocker as a broader command would, stop and hand off instead of burning time on redundant reruns.
- After two failed environment, linker, toolchain, or command attempts with the same root cause, stop retrying variants and return a structured handoff.
- After one model/provider timeout or `429` class failure and one confirmatory retry, stop and return a structured handoff instead of re-asking repeatedly.
