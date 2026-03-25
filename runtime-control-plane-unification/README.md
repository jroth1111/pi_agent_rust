# Runtime Control Plane Unification

**Branch**: `runtime-control-plane-unification`  
**Status**: Planning  
**Created**: 2026-03-22

## Overview

This folder now describes the strongest end-state architecture for `pi_agent_rust` without treating the current repository decomposition as a design boundary.

The target is not a narrower `rpc.rs` extraction and not a single giant `control_plane` module. The target is a system with **separate authoritative cores plus explicit supporting subsystems**:

- `Conversation Engine`
- `Workflow Engine`
- `Inference Plane`
- `Context and Retrieval Plane`
- `Resource and Admission Plane`
- `Host Execution Plane`
- `Workspace Plane`
- `Persistence Plane`
- `Identity and Secrets Plane`
- `Capability and Approval Plane`
- `Provider Adapter Layer`
- `Extension Runtime Plane`
- `Instruction Catalog`
- thin `CLI/TUI/RPC/SDK` surfaces

The current codebase splits authority across `rpc`, `orchestration`, `reliability`, `task_graph`, `state`, hybrid session stores, and a large transitional extension runtime. This plan replaces those overlapping authorities with explicit ownership boundaries.

This revised version also corrects eleven weaknesses in the prior rewrite:

- it rejects a vague “one giant plane does everything” architecture
- it rejects replay-first workflow control and generic event-sourcing absolutism
- it rejects an abstract “typed runtime” story that still leaves production extensions effectively in-process and compatibility-driven
- it rejects fragmented permission and approval logic across tools, workers, providers, and extensions
- it rejects promoting provider, extension, and instruction support concerns into peer control planes when they do not own the product's primary state machines
- it rejects treating Git/worktree state as workflow plumbing instead of a first-class product boundary
- it rejects treating shell/file/network/process execution as conversation or extension implementation detail instead of a first-class host boundary
- it rejects mixing identity, credential materialization, and authorization into one vague policy story
- it rejects scattering local-capacity limits, provider quotas, token budgets, and backpressure policy across engines, providers, and tools instead of giving them one first-class owner
- it rejects treating normalized model execution as mere provider plumbing instead of a first-class inference boundary
- it rejects treating retrieval, indexing, and context-pack assembly as prompt-assembly or tool-loop detail instead of a first-class context boundary

The target is now:

- domain-bounded engines with explicit anti-corruption boundaries
- append-only session history where append-only history is the real domain truth
- transactional workflow control state plus append-only audit/journal history
- a first-class host execution plane for shell, filesystem, process, and network actions
- a first-class workspace plane for repo/worktree ownership, patch application, and merge/replay semantics
- a first-class identity and secrets plane for operator identity, agent identity, provider credentials, and extension secret binding
- one capability and approval system across built-ins, workers, extensions, and provider access
- a first-class inference plane for model catalog, capability normalization, routing/failover, stream/tool-call normalization, and token accounting
- a first-class context and retrieval plane for code/session/artifact indexing, context-pack assembly, provenance, and freshness
- a first-class resource and admission plane for local capacity, provider quotas, token budgets, fairness, queueing, and backpressure
- worker execution through an explicit runtime interface
- out-of-process production extension runtime, with compatibility tooling bounded to migration and test
- provider adapters and instruction catalogs as supporting subsystems, not peer control authorities

## Key Conclusions

1. `rpc.rs` must stop being a business-logic core.
2. Interactive conversation and multi-task workflow automation are different engines and should stay different.
3. Host execution is first-class and needs its own authority boundary instead of hiding inside tool loops or extension runtimes.
4. Workspace and Git semantics are first-class and need their own authority boundary instead of hiding under workflow plumbing.
5. Identity and secrets are first-class and need their own authority boundary instead of leaking through providers, environments, or approval code.
6. Inference is first-class and needs its own authority boundary instead of leaking through conversation code or vendor adapters.
7. Context and retrieval are first-class and need their own authority boundary instead of leaking through prompt assembly, tool loops, or ad hoc grep/read heuristics.
8. Permission answers whether an action may happen; resource admission answers whether it can happen now without violating local capacity, rate limits, or fairness policy.
9. Session persistence should be append-only and rebuildable from history; workflow persistence should be transactionally authoritative with durable audit history.
10. Verification belongs to the workflow engine and persistence plane, not scattered helper modules.
11. Session continuity, skill continuity, and compaction continuity must be typed state, not prompt luck.
12. Permission and approval policy must be unified across built-in tools, workflow workers, extensions, and provider credential use.
13. Raw JS compatibility runtime must not survive as the production default extension architecture.
14. The `Workflow Engine` should launch work through a `WorkerRuntime` interface, not by reaching into conversation internals.
15. One physical SQLite substrate is acceptable, but not one generic catch-all event table and not replay-first workflow control.

## Permanent Target

### Core architecture

- `Conversation Engine`
  Turn lifecycle, prompt assembly, tool loop, compaction, session continuity, skill activation events.
- `Workflow Engine`
  DAGs, runs, tasks, leases, retries, blockers, worktrees, verification policy, and run-level recovery.
- `Inference Plane`
  Canonical model catalog, capability normalization, routing/failover, stream/tool-call normalization, inference receipts, and token accounting.
- `Context and Retrieval Plane`
  Code/session/artifact/instruction indexing, retrieval policy, context-pack assembly, freshness rules, and provenance for turns, workers, compaction, and verification.
- `Resource and Admission Plane`
  Admission decisions, local concurrency budgets, provider rate limits, token budgets, queue classes, fairness, saturation tracking, and backpressure/load-shed policy.
- `Host Execution Plane`
  Shell/file/network/process execution, sandbox/session env binding, streaming stdout/stderr, execution receipts, and execution enforcement after admission.
- `Workspace Plane`
  Repo registry, worktree lifecycle, branch/ref mapping, patch application, diff snapshots, merge/replay/conflict materialization, and cleanup/repair tooling.
- `Persistence Plane`
  Authoritative `Session Event Store`, `Workflow Store`, `Workflow Transition Journal`, `Workspace Registry`, `Workspace Journal`, `Execution Ledger`, `Inference Ledger`, `Context Pack Ledger`, `Identity Registry`, `Resource Registry`, `Admission Journal`, `Approval Ledger`, `Artifact Store`, and rebuildable projections.
- `Identity and Secrets Plane`
  Operator identity, agent identity, provider credential handles, extension secret bindings, secret materialization, and audit-ready identity provenance.
- `Capability and Approval Plane`
  Permission profiles, approval policy, grant issuance, override authorization, and sensitive-action audit hooks.
- `Provider Adapter Layer`
  Vendor protocol implementations, auth/onboarding metadata, provider capability descriptors, and adapter-specific conformance shims.
- `Extension Runtime Plane`
  Extension catalog/build pipeline, runtime host, typed host protocol, and sandbox process model.
- `Instruction Catalog`
  Skills, prompts, instruction bundles, and typed continuity metadata.

### Physical persistence preference

The strongest local deployment shape is:

- append-only session event storage on embedded SQLite WAL
- normalized workflow control tables plus append-only workflow audit/journal storage on embedded SQLite WAL
- workspace registry and workspace journal records on embedded SQLite WAL
- execution ledger and execution audit records on embedded SQLite WAL
- inference ledger and inference audit records on embedded SQLite WAL
- context-pack ledger tables plus rebuildable retrieval indexes on embedded SQLite WAL
- identity registry and secret-handle metadata on embedded SQLite WAL
- resource registry tables plus append-only admission/saturation journal storage on embedded SQLite WAL
- OS-backed secret store or encrypted local vault for secret payloads
- approval and security audit records on embedded SQLite WAL
- content-addressed artifact/blob storage for large payloads
- rebuildable snapshots, projections, and indexes

This keeps local deployment strong without preserving the repo's current hybrid JSONL/V2/direct-SQLite ambiguity.

The intended shape is:

- domain-specific append-only logs for session state
- domain-specific transactional tables plus append-only audit/journal history for workflow control
- domain-specific workspace ownership and lifecycle records
- domain-specific execution receipts and host-action audit history
- domain-specific inference receipts, model-routing history, and token accounting
- domain-specific context-pack provenance and rebuildable retrieval indexes
- domain-specific identity metadata and secret-handle records
- domain-specific resource/quota state plus append-only admission and saturation history
- domain-specific approval records and security audit history
- domain-specific rebuildable snapshot/projection tables for hot reads
- no co-equal truth split between current-state control tables, logs, and projections

## Documents

| Document | Purpose |
|----------|---------|
| [ARCHITECTURE_REVIEW.md](./ARCHITECTURE_REVIEW.md) | Strongest end-state architecture, data boundaries, subsystem dispositions, and target invariants |
| [VERIFICATION_CONSOLIDATION.md](./VERIFICATION_CONSOLIDATION.md) | Verification as a workflow-engine concern backed by durable workflow state, approval-aware overrides, audit history, and artifacts |
| [WORKER_ROLE_ELEVATION.md](./WORKER_ROLE_ELEVATION.md) | Role-aware worker execution model owned by the workflow engine |
| [TASKS.md](./TASKS.md) | Replacement-oriented implementation and cutover plan aligned to the stronger end-state |

## Migration Themes

1. **Thin the surfaces first**  
   CLI/TUI/RPC/SDK call typed services instead of owning business logic.
2. **Create the persistence plane**  
   Authoritative session history, transactional workflow control state, workspace ownership history, execution history, inference history, identity metadata, resource/admission state, approval/security audit history, artifact store, and rebuildable snapshots/projections.
3. **Establish the identity boundary**  
   One identity and secrets plane owns operator identity, agent identity, credential handles, and secret materialization.
4. **Establish the capability boundary**  
   One policy system decides permissions, approvals, and override authority across tools, workers, extensions, and providers.
5. **Establish the resource and admission boundary**  
   One resource and admission plane owns local capacity, provider quotas, token budgets, queueing, fairness, and backpressure.
6. **Establish the inference boundary**  
   One inference plane owns model catalog, capability normalization, routing, stream/tool-call normalization, and token accounting, while provider adapters stay subordinate.
7. **Establish the host execution boundary**  
   One host execution plane owns shell, filesystem, process, and network execution semantics after admission.
8. **Establish the workspace boundary**  
   One workspace plane owns repo/worktree lifecycle, patch application, merge/replay, and repair semantics.
9. **Establish the context and retrieval boundary**  
   One context and retrieval plane owns indexing, retrieval policy, context-pack assembly, provenance, and freshness across code, session, artifact, and instruction sources.
10. **Separate the engines**  
   Conversation Engine and Workflow Engine get explicit ownership boundaries.
11. **Introduce explicit runtime interfaces**  
   Workflow launches work through `WorkerRuntime`; surfaces call application services; model calls cross `InferenceService`; context assembly crosses `ContextService`; providers and extensions cross typed boundaries only.
12. **Place support subsystems correctly**  
   Provider adapters, extension execution, and instruction catalogs stop masquerading as peer control planes and instead plug into explicit boundaries.
13. **Replace transitional runtime paths**  
   Hybrid session storage, duplicate task state, replay-first workflow control, caller-owned throttling and rate-limit logic, engine-owned provider branching, ad hoc context packing/retrieval heuristics, host-execution leakage into engines, worktree-plumbing ownership, ad hoc secret handling, and in-process raw JS extension runtime stop defining production behavior.
14. **Cut over hard and delete**  
   New architecture is incomplete if the old default paths remain live.

## Source Inputs

- direct inspection of current `main`
- earlier `runtime-control-plane-unification` analysis and worker/verification observations
- end-state review in `pi_agent_rust-endstate-review`
- external durable-execution, embedded-persistence, and worktree behavior evidence
