# Validation

Mission-specific validation notes and invariants.

**What belongs here:** Required gates, validation limitations, assertion coverage rules, evidence expectations.

---

## Required automated gates

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`

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
