# Architecture

Architectural decisions and patterns for this mission.

**What belongs here:** Domain ownership boundaries, anti-corruption rules, cutover expectations, important invariants.
**What does NOT belong here:** Per-feature implementation logs.

---

## Mission architectural target

This mission implements the `runtime-control-plane-unification` plan as a hard-cutover architecture modernization.

Primary authorities after cutover:
- Conversation Engine
- Workflow Engine
- Persistence Plane
- Inference Plane
- Context and Retrieval Plane
- Resource and Admission Plane
- Host Execution Plane
- Workspace Plane
- Identity and Secrets Plane
- Capability and Approval Plane

Supporting subsystems, not peer authorities:
- Provider Adapter Layer
- Extension Runtime Plane
- Instruction Catalog
- thin CLI/TUI/RPC/SDK surfaces

## Non-negotiable rules

- Breaking changes are allowed.
- No backwards compatibility is required.
- Do not preserve hybrid truth as a permanent compromise.
- Surfaces must become adapters, not business-logic owners.
- Projections are derived state only.
- Transitional default paths must be deleted or quarantined from normal startup after cutover.

## Current decomposition plan

Milestone 1: surface-thinning-and-contracts
Milestone 2: authoritative-state-core
Milestone 3: workflow-engine-unification
Milestone 4: shared-runtime-boundaries
Milestone 5: cutover-and-subsystem-placement
