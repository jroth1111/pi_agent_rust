# Runtime Control Plane Unification - Task Breakdown

**Project**: runtime-control-plane-unification  
**Branch**: `runtime-control-plane-unification`  
**Created**: 2026-03-22

## Delivery Principle

This plan is replacement-oriented. It is complete only when the new engines and planes materially replace the current default runtime paths.

The target is complete only when:

- surfaces are thin
- one Conversation Engine owns interactive session behavior
- one Workflow Engine owns task/run behavior
- one Inference Plane owns normalized model execution, routing, and token accounting
- one Context and Retrieval Plane owns retrieval policy, indexing, context-pack assembly, and provenance
- one Resource and Admission Plane owns queueing, quotas, and backpressure behavior
- one Host Execution Plane owns shell/file/network/process execution behavior
- one Workspace Plane owns repo/worktree mutation and recovery behavior
- one Identity and Secrets Plane owns identity, credential, and secret materialization behavior
- one Persistence Plane owns authoritative stores and projections
- one Capability and Approval Plane owns privileged-action policy
- domain-specific logs and snapshots exist instead of a generic catch-all event table
- workflow control is transactionally authoritative and not replay-first
- extension production runtime no longer defaults to raw JS compatibility
- skill continuity survives compaction and resume as typed state
- provider adapters and instruction assets do not survive as peer control authorities
- caller-owned throttling, queueing, and rate-limit logic no longer remain authoritative
- engine-owned provider branching and token-accounting logic no longer remain authoritative
- prompt-owned or tool-owned context assembly and provenance no longer remain authoritative

## Phase 1: Freeze The End-State Contracts And Thin The Surfaces

### Task 1.1: Define permanent service boundaries
**Priority**: High | **Risk**: Medium

Define the typed contracts for:

- `ConversationService`
- `WorkflowService`
- `InferenceService`
- `ContextService`
- `ResourceAdmissionService`
- `HostExecutionService`
- `WorkspaceService`
- `IdentityService`
- `CapabilityService`
- `PersistencePlane`
- `ProviderAdapterLayer`
- `ExtensionRuntimeHost`
- `InstructionCatalog`
- `WorkerRuntime`

**Acceptance Criteria**:

- [ ] service boundaries are documented and code-targetable
- [ ] surfaces no longer invent separate business contracts
- [ ] `WorkerRuntime` boundary is defined so workflow does not depend on conversation internals
- [ ] `InferenceService` boundary is defined so model capability, routing, and stream normalization do not leak into engines
- [ ] `ContextService` boundary is defined so retrieval policy, indexing, and context-pack assembly do not leak into engines or tools
- [ ] `ResourceAdmissionService` boundary is defined so budgets and queueing do not leak into engines or adapters
- [ ] provider adapters, extension execution, and instruction catalogs are not specified as peer control authorities

### Task 1.2: Freeze dependency direction and consistency rules
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] allowed engine/plane dependency directions are documented
- [ ] workflow command idempotency rules are documented
- [ ] admission, release, rebudget, and saturation rules are documented
- [ ] inference receipt, routing, and token-accounting rules are documented
- [ ] context-pack provenance, freshness, and rebuild rules are documented
- [ ] transaction boundaries for host execution writes and receipt capture are documented
- [ ] transaction boundaries for inference writes and usage capture are documented
- [ ] transaction boundaries for context-pack writes and retrieval-index rebuilds are documented
- [ ] transaction boundaries for session writes, workflow writes, and artifact attachment are documented
- [ ] transaction boundaries for identity metadata writes and secret-handle registration are documented
- [ ] transaction boundaries for resource/quota state and admission journal writes are documented
- [ ] transaction boundaries for workspace writes and repair flows are documented
- [ ] no end-state path depends on replaying workflow history for hot-path control decisions

### Task 1.3: Thin CLI/TUI/RPC/SDK surfaces
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] `rpc` becomes transport-oriented
- [ ] startup wiring becomes composition-only
- [ ] CLI/TUI/RPC/SDK all route to the same service contracts

## Phase 2: Build The Persistence Plane

### Task 2.1: Create authoritative Session Event Store
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] append-only session event envelopes exist
- [ ] compaction, branch, model-change, and skill-activation state are representable as typed events
- [ ] JSONL is no longer the end-state source of truth
- [ ] hot-read snapshot/projection tables exist and are rebuildable from session events

### Task 2.2: Create authoritative Workflow Store and transition journal
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] task/run/lease/retry/blocker/verification current state lives in transactional workflow tables
- [ ] workflow mutations append audit/journal rows atomically with current-state updates
- [ ] duplicate task lifecycle models are not preserved as co-equal authorities
- [ ] hot-read snapshot/projection tables exist and are rebuildable from workflow state and audit history
- [ ] runnable-task selection, lease ownership, and recovery do not depend on replaying full workflow history

### Task 2.3: Create Workspace Registry and workspace journal
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] repo/worktree ownership and lifecycle state are durably recorded
- [ ] patch apply, merge/replay, and repair actions append workspace journal history
- [ ] live worktree truth does not depend only on Git admin metadata or process memory

### Task 2.4: Create Execution Ledger and execution audit storage
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] shell/file/network/process execution receipts are durably recorded
- [ ] timeout, cancel, and failure outcomes are queryable from durable state
- [ ] built-in tools, workers, and extension executions can reference execution records

### Task 2.5: Create Inference Ledger and inference audit storage
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] normalized inference request/response receipts are durably recorded
- [ ] model/profile selection, routing decisions, token usage, and outcome classification are queryable from durable state
- [ ] engines and verifiers do not need provider-specific logs to explain model-call behavior

### Task 2.6: Create Identity Registry and secret-handle metadata
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] operator identities, agent identities, and credential handles are durably recorded
- [ ] provider and extension secret bindings can be referenced without storing raw secret payloads in generic app tables
- [ ] approval actors and runtime secret materialization can reference identity records

### Task 2.7: Create Resource Registry and admission journal
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] local concurrency budgets, provider quota state, token-budget policy, and queue classes are durably recorded
- [ ] admission grants, denials, deferrals, releases, and saturation events append immutable journal history
- [ ] queueing and throttling state are not left to caller-local memory or provider-specific side tables

### Task 2.8: Create Approval Ledger and security audit storage
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] approval requests, decisions, grant issuance, and revocations are durably recorded
- [ ] security-relevant audit events are durable and queryable
- [ ] verification overrides and privileged worker launches can reference approval records

### Task 2.9: Create Artifact Store and rebuildable projections
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] artifact blobs are durable and content-addressed
- [ ] projections are rebuildable from authoritative stores
- [ ] SQLite catalogs/indexes are derived, not authoritative
- [ ] no generic cross-domain catch-all event table becomes the default persistence pattern

### Task 2.10: Create Context Pack Ledger and rebuildable retrieval indexes
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] context-pack identities, provenance refs, freshness markers, and usage bindings are durably recorded
- [ ] retrieval indexes over workspace/session/artifact/instruction sources are rebuildable from authoritative stores and workspace state
- [ ] turns, worker launches, compaction, and verification paths can reference context packs instead of opaque prompt-only bundles

## Phase 3: Build The Identity And Secrets Plane

### Task 3.1: Unify identity and credential ownership
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] provider credentials, extension secrets, and approval actor identity resolve through one identity/secrets system
- [ ] raw secret payloads are not treated as ordinary config or generic database payloads
- [ ] ad hoc env/config-based secret handling no longer remains authoritative

### Task 3.2: Make secret materialization and provenance durable
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] runtimes, inference services, and provider adapters receive secret handles or materialized bindings through one identity service
- [ ] credential rotation/revocation metadata is inspectable
- [ ] privileged actions can be traced to durable actor identity records

## Phase 4: Build The Capability And Approval Plane

### Task 4.1: Unify permission and approval policy
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] built-in tools, workflow workers, extensions, and provider credential access resolve through one capability system
- [ ] approval requirements are policy-driven rather than ad hoc per subsystem
- [ ] privileged overrides are policy-controlled

### Task 4.2: Make grants and approvals durable
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] capability grants can be durably referenced by worker launches and extension executions
- [ ] approval decisions survive crash/restart and are inspectable
- [ ] no surface-specific permission path remains authoritative

## Phase 5: Build The Resource And Admission Plane

### Task 5.1: Unify local capacity, provider quotas, and queueing policy
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] interactive turns, workflow launches, verification work, provider requests, and extension executions request admission through one resource system where policy requires it
- [ ] local concurrency budgets, provider rate limits, token budgets, and fairness tiers are centralized
- [ ] ad hoc caller-owned throttling and queueing no longer remain authoritative

### Task 5.2: Make admission decisions and saturation durable
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] admission grants, denials, deferrals, releases, and saturation markers survive crash/restart and are inspectable
- [ ] provider cooldowns and rebudget decisions are queryable from durable state
- [ ] load-shed and backpressure behavior are explainable rather than hidden in retry loops

## Phase 6: Build The Inference Plane

### Task 6.1: Unify model capability normalization, routing, and receipts
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] model capability descriptors, routing/failover rules, and stream/tool-call normalization resolve through one inference system
- [ ] ad hoc engine-owned provider branching no longer remains authoritative
- [ ] provider adapters are subordinate to the inference boundary rather than acting as the product-facing model contract

### Task 6.2: Make token accounting and inference outcomes durable
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] token usage, context-fit decisions, routing choices, and outcome classification are inspectable from durable inference state
- [ ] engines consume normalized inference results instead of provider-specific shapes
- [ ] retry/failover policy for model calls is explainable from one inference system

## Phase 7: Build The Host Execution Plane

### Task 7.1: Unify shell, file, network, and process execution
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] built-in tools, worker runtimes, and extensions resolve host execution through one execution system
- [ ] timeout, cancellation, and execution-environment limit semantics are centralized
- [ ] ad hoc shell/file/network/process execution paths no longer remain authoritative

### Task 7.2: Make execution receipts and enforcement durable
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] execution receipts survive crash/restart and are inspectable
- [ ] streamed output and failure classification can be traced from durable state
- [ ] host execution semantics are consistent across interactive and workflow execution after admission

## Phase 8: Build The Workspace Plane

### Task 8.1: Unify repo and worktree ownership
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] repo discovery, worktree lifecycle, and branch/ref mapping resolve through one workspace system
- [ ] merge/replay/conflict materialization policy is centralized
- [ ] ad hoc repo mutation paths no longer remain authoritative

### Task 8.2: Make repair and cleanup first-class
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] workspace repair and cleanup commands are explicit and inspectable
- [ ] worktree recovery can be explained from durable workspace state
- [ ] workspace mutation semantics are consistent across interactive and workflow execution

## Phase 9: Build The Context And Retrieval Plane

### Task 9.1: Unify retrieval across workspace, session, artifact, and instruction sources
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] code, session, artifact, and instruction retrieval resolve through one context system
- [ ] ad hoc grep/read-driven prompt assembly no longer remains authoritative
- [ ] freshness and staleness policy are centralized

### Task 9.2: Make context-pack assembly and provenance durable
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] conversation turns, worker launches, compaction, and verification can bind to durable context-pack refs
- [ ] context-pack provenance is inspectable
- [ ] retrieval indexes and context-pack identities can be rebuilt or rehydrated without prompt reconstruction

## Phase 10: Extract The Conversation Engine

### Task 10.1: Separate conversation behavior from transport, workflow, provider specifics, and ad hoc retrieval
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] turn lifecycle, prompt assembly, tool loop, and compaction logic are owned by the Conversation Engine
- [ ] model execution routes through the `Inference Plane` instead of provider-specific branches
- [ ] context assembly routes through the `Context and Retrieval Plane` instead of ad hoc tool- or prompt-owned logic
- [ ] workflow/DAG semantics no longer live inside the conversation path
- [ ] Conversation Engine exposes worker execution only through the `WorkerRuntime` interface

### Task 10.2: Make skill continuity typed
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] skill activation is stored as typed session events
- [ ] active or invoked skills survive compaction and resume
- [ ] prompt-only skill continuity is eliminated as the default mechanism
- [ ] instruction continuity does not depend on executable extension state

## Phase 11: Build The Workflow Engine

### Task 11.1: Merge duplicated task/run state into one workflow engine
**Priority**: High | **Risk**: High

Targets:

- RPC-owned orchestration state
- `src/reliability/task.rs`
- `src/task_graph/`
- `src/state/`
- `src/orchestration/run.rs`

**Acceptance Criteria**:

- [ ] one authoritative workflow state machine exists
- [ ] workflow transitions are validated in one place
- [ ] JSON run-store authority is removed
- [ ] workflow commands are idempotent at the persistence boundary

### Task 11.2: Route workspace operations through the Workspace Plane
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] worktree provisioning, patch application, and merge/replay requests go through the Workspace Plane
- [ ] workflow no longer owns workspace lifecycle truth
- [ ] lease/worktree ownership is transactionally fenced and workspace-backed

### Task 11.3: Add role-aware worker launch model
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] planner/implementor/reviewer/verifier roles are durable
- [ ] launches record role, runtime kind, admission class, permissions, context-pack refs, and verification policy
- [ ] generic default-session worker launches are no longer the orchestration default
- [ ] workflow launches work only through `WorkerRuntime`
- [ ] privileged worker actions reference the capability and approval system
- [ ] worker launches request admission through the resource and admission system

## Phase 12: Unify Verification

### Task 12.1: Make verification a Workflow Engine concern
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] Workflow Engine owns completion policy
- [ ] verification results are durable workflow-linked records
- [ ] privileged overrides are governed through the capability and approval system
- [ ] `src/reliability/verifier.rs`, `src/verification/`, and `src/state/gates.rs` stop being live authorities

### Task 12.2: Build verification projections and inspectors
**Priority**: Medium | **Risk**: Medium

**Acceptance Criteria**:

- [ ] task/run verification summaries are projection-backed
- [ ] operators can inspect why a completion decision was made

## Phase 13: Replace Session Persistence

### Task 13.1: Cut over session runtime behavior to Session Event Store
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] create/open/save/resume/compact operate through the Session Event Store
- [ ] JSONL and direct SQLite survive only as migration import/export paths
- [ ] V2 no longer survives as a permanent co-equal sidecar concept

### Task 13.2: Replace picker and `get_state` with projection-backed reads
**Priority**: Medium | **Risk**: Medium

**Acceptance Criteria**:

- [ ] picker reads derived catalog/projection data
- [ ] RPC `get_state` and SDK state read derived projections
- [ ] surfaces do not reconstruct truth from hybrid storage paths

## Phase 14: Place Support Subsystems Correctly

### Task 14.1: Demote provider adapters beneath the inference plane
**Priority**: Medium | **Risk**: Medium

**Acceptance Criteria**:

- [ ] provider metadata and protocol shims serve the inference plane through explicit interfaces
- [ ] provider adapters do not own secret payload storage or materialization
- [ ] provider adapters do not own durable rate-limit or quota state
- [ ] provider adapters do not define the product-facing model contract or routing behavior

### Task 14.2: Separate instruction catalogs from executable extension runtime
**Priority**: High | **Risk**: Medium

**Acceptance Criteria**:

- [ ] skills/prompts/instruction bundles live in a declarative catalog
- [ ] executable extensions live in a separate runtime subsystem
- [ ] instruction continuity does not rely on extension runtime state

## Phase 15: Replace The Extension Runtime

### Task 15.1: Build the typed production extension runtime path
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] extension bundle/import pipeline exists
- [ ] production execution routes through typed out-of-process runtime artifacts
- [ ] hostcall policy is singular and inspectable
- [ ] hostcall policy is enforced across a typed runtime boundary, not by trusting in-process compatibility execution
- [ ] production extension format is explicit and typed, not inferred from compatibility execution
- [ ] extension grants flow through the capability and approval system
- [ ] extension secret binding flows through the identity and secrets plane
- [ ] extension execution admission flows through the resource and admission plane

### Task 15.2: Demote raw JS compatibility runtime to bounded migration tooling
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] raw JS compatibility is build/test/import support only
- [ ] direct raw JS execution is absent from the production default path
- [ ] raw JS compatibility is not the production default path

## Phase 16: Migration Tooling And Hard Cutover

### Task 16.1: Build migration and inspection tooling
**Priority**: High | **Risk**: Medium

Required capabilities:

- session migration verifier
- workflow migration verifier
- workspace migration verifier
- execution ledger migration verifier
- inference migration verifier
- context/retrieval migration verifier
- identity/credential migration verifier
- resource/admission migration verifier
- approval/grant migration verifier
- projection rebuild tooling
- session/run inspectors
- extension bundle diagnostics
- store-aware `doctor`

**Acceptance Criteria**:

- [ ] these tools exist and are used during cutover

### Task 16.2: Hard routing switch
**Priority**: High | **Risk**: High

**Acceptance Criteria**:

- [ ] new engines and stores are the default runtime path
- [ ] shadowing is disabled after cutover
- [ ] legacy paths no longer materially define runtime behavior

### Task 16.3: Delete transitional paths
**Priority**: High | **Risk**: High

Delete or quarantine after cutover:

- JSONL primary session persistence
- direct `.sqlite` session runtime path
- V2 sidecar as co-equal runtime authority
- RPC-owned workflow logic
- duplicate task lifecycle models
- replay-first workflow control
- ad hoc throttling, queueing, or provider rate-limit state
- ad hoc provider/model routing, capability branching, or token-accounting logic
- ad hoc context packing, retrieval, or freshness heuristics
- ad hoc shell/file/network/process execution paths
- ad hoc repo/worktree mutation paths
- ad hoc identity/credential/env-secret handling
- fragmented tool/worker/extension/provider approval logic
- provider adapters as a peer control authority
- instruction assets coupled to executable extension runtime
- production-default raw JS compatibility runtime
- any generic catch-all event table or co-equal projection truth introduced during migration

**Acceptance Criteria**:

- [ ] old paths are removed or clearly migration-only
- [ ] final verification passes after deletion

## Suggested Verification Commands

During implementation:

```bash
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Targeted verification slices should be added per completed phase:

- session-store tests
- workflow transition tests
- inference normalization, routing, and token-accounting tests
- context retrieval, provenance, and freshness tests
- resource admission, fairness, and quota tests
- host execution and receipt tests
- workspace lifecycle and recovery tests
- identity/credential binding and revocation tests
- capability/approval policy tests
- RPC/SDK adapter tests
- provider adapter conformance tests
- migration/import verifier runs
- extension bundle/runtime tests

## Progress Ledger

| Phase | Item | Status |
|------|------|--------|
| 1 | Surfaces thinned and service contracts frozen | Not Started |
| 2 | Persistence Plane built | Not Started |
| 3 | Identity and Secrets Plane built | Not Started |
| 4 | Capability and Approval Plane built | Not Started |
| 5 | Resource and Admission Plane built | Not Started |
| 6 | Inference Plane built | Not Started |
| 7 | Host Execution Plane built | Not Started |
| 8 | Workspace Plane built | Not Started |
| 9 | Context and Retrieval Plane built | Not Started |
| 10 | Conversation Engine extracted | Not Started |
| 11 | Workflow Engine built | Not Started |
| 12 | Verification unified | Not Started |
| 13 | Session persistence replaced | Not Started |
| 14 | Support subsystems correctly placed | Not Started |
| 15 | Extension runtime replaced | Not Started |
| 16 | Migration tooling and hard cutover complete | Not Started |
