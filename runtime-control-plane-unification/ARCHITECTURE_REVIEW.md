# Runtime Control Plane Unification - Architecture Review

**Date**: 2026-03-22  
**Branch**: `runtime-control-plane-unification`

## Executive Summary

The strongest end-state for `pi_agent_rust` is not a monolithic control-plane refactor. It is a system with **separate authoritative engines and planes plus explicit supporting subsystems**:

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

This is stronger than both the original checked-in `runtime/*` extraction plan and the intermediate rewritten `control_plane` plan because it aligns control ownership with the product's real responsibilities instead of centralizing everything into one oversized subsystem or inflating support concerns into peer control planes.

It is also stronger than a pure “event everything” redesign because it distinguishes domains where append-only history is the natural source of truth from domains where transactional current state must remain canonical.

## Decision Rule

The architecture should be chosen to maximize:

1. mission success under real local coding workflows
2. correctness
3. security
4. maintainability and operability
5. simplicity after accidental complexity is removed

The rule is: prefer explicit authority boundaries and durable typed state, even when they require more up-front structure than a narrower refactor.

## Self-Critique Of The Previous Revision

The previous revision improved the original checked-in plan, but it still had twelve logical weaknesses:

### 1. It risked replacing one accidental monolith with another

Calling everything a `Persistence Plane` or `control plane` is not enough. Without explicit anti-corruption layers and runtime interfaces, a “plane” can become just another global dumping ground.

### 2. It risked event-sourcing absolutism

“Everything is an event” is not, by itself, a strong design. The best solution is:

- append-only domain logs where ordered history is the domain truth
- transactional current-state tables where leases, retries, worktree ownership, and recovery need canonical hot state
- rebuildable snapshots and projections for hot reads and fast startup
- explicit rejection of a single generic catch-all event table

In this project, session history is naturally append-only. Workflow control is not best modeled as replay-first event sourcing.

### 3. It under-specified how workflow execution invokes work

The best architecture does not let the Workflow Engine reach into conversation internals ad hoc. It should depend on a `WorkerRuntime` interface implemented by the Conversation Engine and any future specialized runtimes.

### 4. It still left the production extension runtime too abstract

Saying “typed runtime” is not enough if the effective production model is still in-process compatibility execution. The stronger end-state is an out-of-process extension runtime with explicit manifests, capability policy, and a typed host protocol.

### 5. It still fragmented policy and approval authority

Extension hostcall policy alone is not enough. Built-in tools, workflow worker launches, provider credential use, and manual overrides all cross security boundaries. The strongest end-state has one `Capability and Approval Plane`, not scattered policy checks.

### 6. It still gave peer status to support subsystems that do not own primary product state

Provider adaptation, extension execution, and instruction catalogs matter, but they are not all peer control authorities. Treating them that way adds coordination burden without improving correctness.

### 7. It still under-modeled workspace ownership

For a coding agent, Git repos, worktrees, diffs, patch application, merge/replay, and repair are not incidental plumbing. Treating them as workflow implementation detail hides a primary correctness boundary.

### 8. It still under-modeled host execution

Shell commands, file reads/writes, process lifecycle, network access, and streaming execution receipts are not just “tool implementation.” They are a primary security, correctness, and operability boundary.

### 9. It still under-modeled identity and secrets

Authorization is not enough. The system also needs a clear authority for operator identity, agent identity, provider credential materialization, extension secret binding, and approval provenance.

### 10. It still under-modeled resource admission and saturation

Authorization does not answer whether the system should admit more work right now. Without one explicit owner for local concurrency budgets, provider quotas, token budgets, fairness tiers, queueing, and backpressure, those decisions leak into workflow code, provider adapters, tool runtimes, and transport loops.

### 11. It still under-modeled inference semantics

Provider adapters are not the same thing as product-owned inference behavior. Without one explicit owner for model capability normalization, routing, token accounting, stream/tool-call normalization, and provider-failure classification, those concerns leak into conversation code, provider-specific branches, and retry heuristics.

### 12. It still under-modeled context assembly and provenance

Context is not just prompt text. Without one explicit owner for indexing, retrieval policy, context-pack assembly, freshness, and provenance across code, sessions, artifacts, and instructions, those concerns leak into prompt assembly, tool loops, compaction heuristics, and worker launch code.

## Current Structural Failures

### 1. Too many live authorities

Current `main` spreads execution meaning across:

- `src/rpc.rs`
- `src/orchestration/run.rs`
- `src/reliability/task.rs`
- `src/task_graph/`
- `src/state/`

This is not domain complexity. It is authority duplication.

### 2. Hybrid session persistence

Session behavior is currently defined by a mix of:

- JSONL primary files
- V2 segmented sidecars
- direct SQLite sessions
- a separate SQLite session index

That is not a coherent end-state.

### 3. Transport owns application logic

`src/rpc.rs` still owns task contracts, dispatch behavior, workflow semantics, and state shaping. That makes RPC a second application core instead of a transport surface.

### 4. Workflow and conversation semantics are entangled

Interactive turn execution and DAG/task orchestration are related but not identical. Treating them as one control object weakens both.

### 5. Extension runtime is still transitional

The current extension architecture still treats raw JS compatibility runtime as a real production path instead of bounded migration scaffolding.

### 6. Skill continuity is not yet a first-class state model

Skill activation and continuity still depend too much on prompt and compaction luck rather than durable typed state.

### 7. Permission and approval logic is fragmented

Permission checks are spread across built-in tools, workflow launch semantics, provider setup, and extension policy. That weakens security and makes behavior harder to explain consistently across surfaces.

### 8. Workspace state lacks a first-class authority

Worktree lifecycle, branch/ref mapping, dirty-state capture, patch application, merge/replay, and repair semantics are spread between orchestration and ad hoc Git/tool behavior instead of living under one explicit workspace owner.

### 9. Host execution state lacks a first-class authority

Built-in tools, extension hostcalls, and worker runtimes all need shell/file/network/process access, but execution semantics, receipt capture, and failure handling are not yet owned by one explicit subsystem.

### 10. Identity and credential state lacks a first-class authority

Provider keys, extension secrets, approval actors, and agent/operator identity are still implicit in provider code, environment inheritance, or policy paths rather than living under one explicit owner.

### 11. Resource admission and saturation state lack a first-class authority

Local host saturation, provider rate limits, token budgets, fairness policy, and queueing decisions are still prone to ending up as ad hoc throttles in workflow scheduling, provider adapters, tool runtimes, or retry loops instead of being owned by one explicit subsystem.

### 12. Inference semantics lack a first-class authority

Model capability detection, routing/failover policy, token accounting, stream normalization, and provider-failure classification are still too likely to be treated as conversation details or provider-specific integration logic rather than one product-owned boundary.

### 13. Context and retrieval state lack a first-class authority

Code/session/artifact indexing, retrieval policy, context-pack provenance, and freshness rules are still too likely to be treated as prompt-building detail or ad hoc tool behavior rather than one explicit subsystem.

## End-State Architecture

### 1. Surfaces

`CLI`, `TUI`, `RPC`, and `SDK` are adapters only.

They:

- accept user or programmatic input
- translate to typed service calls
- stream typed events and results back

They do not:

- own business state
- define workflow contracts
- enforce workflow transitions
- define persistence truth

Surfaces should cross into the rest of the system through typed application service interfaces only.

### Dependency direction

The permanent dependency direction should be:

- surfaces -> application services -> engines and planes
- `Conversation Engine` -> `Inference Plane`, `Context and Retrieval Plane`, `Instruction Catalog`, `Identity and Secrets Plane`, `Capability and Approval Plane`, `Resource and Admission Plane`, `Host Execution Plane`, `Workspace Plane`, `Session Event Store`, `Artifact Store`
- `Workflow Engine` -> `Workflow Store`, `WorkerRuntime`, `Context and Retrieval Plane`, `Identity and Secrets Plane`, `Capability and Approval Plane`, `Resource and Admission Plane`, `Host Execution Plane`, `Workspace Plane`, `Artifact Store`
- `Inference Plane` -> `Identity and Secrets Plane`, `Resource and Admission Plane`, `Provider Adapter Layer`, `Inference Ledger`, `Artifact Store`
- `Context and Retrieval Plane` -> `Inference Plane`, `Workspace Plane`, `Session Event Store`, `Artifact Store`, `Instruction Catalog`, `Context Pack Ledger`
- `Resource and Admission Plane` -> `Resource Registry`, `Admission Journal`, and durable saturation/quota state
- `Host Execution Plane` -> `Resource and Admission Plane`, local shell/filesystem/process/network substrate, `Execution Ledger`, `Artifact Store`
- `Workspace Plane` -> `Resource and Admission Plane`, git/worktree substrate through `Host Execution Plane`, `Workspace Registry`, `Workspace Journal`, `Artifact Store`
- `Extension Runtime Plane` -> typed host protocol + `Identity and Secrets Plane` + `Capability and Approval Plane` + `Resource and Admission Plane`
- `Provider Adapter Layer` -> `Identity and Secrets Plane`, provider protocol implementations, provider metadata, auth scheme definitions
- `Capability and Approval Plane` -> `Identity and Secrets Plane` for subject identity and secret-access decisions
- persistence components do not depend on engines

What should be rejected:

- surfaces constructing engine internals directly
- `Workflow Engine` depending on `Conversation Engine` internals instead of `WorkerRuntime`
- engines branching on provider quirks or model capability details instead of going through `Inference Plane`
- engines assembling context packs or retrieval policy ad hoc instead of going through `Context and Retrieval Plane`
- engines, providers, or extensions each inventing their own queueing, throttling, or rate-limit state
- engines directly spawning shell/process/file/network actions instead of going through `Host Execution Plane`
- `Workflow Engine` or `Conversation Engine` directly mutating repo/worktree state instead of going through `Workspace Plane`
- secret payloads leaking through ad hoc env/config files instead of one identity/secrets system
- provider or extension routing branching inside transport code
- approval policy duplicated in multiple engines or surfaces
- persistence code depending back on application engines

### 2. Conversation Engine

This is the single authoritative engine for interactive coding sessions.

It owns:

- turn lifecycle
- prompt assembly over context packs produced by the `Context and Retrieval Plane`
- tool loop and turn-level inference orchestration through the `Inference Plane`
- compaction decisions and rebuild
- session event emission
- skill activation state
- continuity artifacts needed for resume

It does not own:

- DAG validation
- leases and retries
- run-level verification policy
- worktree scheduling

Recommended internal decomposition:

- `TurnOrchestrator`
- `PromptAssembler`
- `ToolLoop`
- `CompactionManager`
- `ContinuityManager`
- `SkillActivationManager`

### 3. Workflow Engine

This is the single authoritative engine for automation, DAG execution, parallel work, and run-level control.

It owns:

- task specs and prerequisites
- run scheduling and dispatch policy
- leases and fence semantics
- retries, blockers, and awaiting-human states
- worktree provisioning requests
- verification requirements and completion decisions
- worker-role policy

It may launch work through the Conversation Engine or through specialized worker execution paths, but it is the authority over workflow state.

Recommended internal decomposition:

- `RunScheduler`
- `DagValidator`
- `LeaseManager`
- `RetryAndBlockerManager`
- `WorkerLaunchManager`
- `VerificationManager`
- `RecoveryManager`
- `WorktreeExecutionCoordinator`

### Worker runtime boundary

The Workflow Engine should depend on a `WorkerRuntime` interface, not on conversation implementation details.

That interface should cover:

- launch request
- context bundle input
- streamed execution events
- completion payload
- handoff or blocker signaling

The `Conversation Engine` should implement one `WorkerRuntime`. Future specialized runtimes may implement others.

### 4. Resource and Admission Plane

This is the single authoritative boundary for workload admission, queueing, budgets, and saturation policy.

It owns:

- local concurrency budgets for interactive turns, background runs, verification, and maintenance work
- provider request budgets, rate-limit state, and token-budget policy
- fairness tiers and queue classes across interactive, workflow, verifier, and background work
- admission grants, deferrals, denials, and load-shed decisions
- saturation signals and backpressure policy shared across engines and supporting subsystems
- reservation and release semantics for admitted work

It does not own:

- authorization decisions
- identity or secret materialization
- workflow correctness rules
- actual shell/process/file/network execution
- provider adapter implementation details

The strongest design distinguishes three questions cleanly:

- identity: who is acting and with which credential handle?
- capability: may this action happen?
- admission: can this action happen now without violating resource, quota, or fairness constraints?

### 5. Inference Plane

This is the single authoritative boundary for model execution and normalized inference semantics.

It owns:

- canonical model catalog and capability descriptors used by the product
- normalized inference request/response contracts
- provider-neutral streaming delta and tool-call normalization
- routing, failover, and provider-selection policy
- token accounting and context-window-fit decisions at the inference boundary
- inference receipts, provider-failure classification, and retry classification for model calls

It does not own:

- conversation turn state
- workflow scheduling
- credential ownership
- quota ownership
- provider secret storage

The goal is not to wrap providers cosmetically. The goal is to stop provider quirks, model capability branches, and token-accounting logic from leaking into engines.

### 6. Context and Retrieval Plane

This is the single authoritative boundary for retrieval policy, indexing, and context-pack provenance.

It owns:

- normalized context-unit extraction from workspace, session, artifact, and instruction sources
- lexical, structural, and optional semantic retrieval indexes
- context-pack assembly for turns, worker launches, compaction, and verification
- provenance refs, freshness windows, and staleness detection for retrieved context
- reusable context-pack identities that can be referenced from sessions, runs, and verifier actions

It does not own:

- prompt wording or final prompt assembly
- workflow scheduling
- model execution itself
- repo/worktree mutation
- the underlying source-of-truth stores for sessions, workspace, or artifacts

The goal is not to bolt on generic “RAG.” The goal is to stop context selection, packing, and provenance from leaking into prompt assembly, tool loops, and ad hoc grep/read heuristics.

### 7. Persistence Plane

This plane owns durable authoritative state plus rebuildable projections.

Logical authoritative components:

- `Session Event Store`
- `Workflow Store`
- `Workflow Transition Journal`
- `Workspace Registry`
- `Workspace Journal`
- `Execution Ledger`
- `Inference Ledger`
- `Context Pack Ledger`
- `Identity Registry`
- `Resource Registry`
- `Admission Journal`
- `Approval Ledger`
- `Artifact Store`

Derived stores:

- session/run/search catalogs
- context/retrieval indexes
- picker/search projections
- operational diagnostics projections

Strong physical implementation preference for this product:

- embedded SQLite WAL as the local event-store substrate
- append-only session event envelopes with schema ids
- normalized workflow current-state tables with atomic versioned updates
- append-only workflow transition journal rows
- normalized workspace registry tables plus append-only workspace journal rows
- normalized execution receipt tables plus append-only execution audit rows
- normalized inference receipt tables plus append-only inference audit rows
- normalized context-pack tables plus provenance rows and rebuildable retrieval indexes
- normalized identity and secret-handle metadata tables
- normalized resource/quota tables plus append-only admission and saturation journal rows
- append-only approval and security audit rows
- content-addressed artifacts for large payloads

This is not “one giant application table set,” and it is not “one generic event table for everything.” It is one local persistence substrate hosting several authoritative logical stores.

### Persistence self-discipline

The strongest design uses:

- domain-specific append-only event logs
- domain-specific workflow control tables
- domain-specific workspace ownership and lifecycle tables
- domain-specific execution receipt and host-action tables
- domain-specific inference receipt, routing, and token-accounting tables
- domain-specific context-pack provenance and retrieval indexes
- domain-specific identity metadata and secret-handle records
- domain-specific resource/quota state and admission records
- domain-specific approval and grant records
- domain-specific snapshot/projection tables
- rebuild tools that regenerate projections from logs and audit journals where appropriate

What should be rejected:

- generic polymorphic catch-all event rows for unrelated domains
- projections treated as co-equal sources of truth
- replaying full event history on every hot-path read
- making lease ownership or runnable-task selection depend on replaying workflow history
- shell/file/network/process semantics split across tools, worker runtimes, and extensions without one host execution owner
- mutating repo/worktree state from multiple engines without one canonical workspace owner
- provider and extension secrets stored or materialized without one canonical identity/secrets owner
- queueing, rate-limit, or backpressure policy split across workflow, providers, tools, and extensions without one admission owner
- model capability, routing, or stream-normalization logic split across engines and provider adapters without one inference owner
- retrieval policy, context-pack provenance, or freshness logic split across engines and tools without one context owner
- scattering approval decisions into tools, extensions, and workflow code without one authoritative policy plane

### 8. Host Execution Plane

This is the single authoritative boundary for shell, filesystem, process, and network execution.

It owns:

- built-in tool execution runtime
- process lifecycle and streaming stdout/stderr capture
- file read/write execution semantics outside pure repo-state modeling
- network call execution for tools and extension hostcalls
- timeout, cancellation, and execution-environment limit enforcement after admission
- execution receipts and failure classification

It does not own:

- conversation turn state
- workflow scheduling
- repo/worktree ownership semantics
- approval policy

The goal is not to build an “OS abstraction layer.” The goal is to make host actions consistent, governable, and inspectable across built-in tools, workers, and extensions once the work has been admitted.

### 9. Workspace Plane

This is the single authoritative boundary for repository and worktree state.

It owns:

- repo registry and discovery
- linked worktree lifecycle
- branch/ref mapping used by the product
- base commit and dirty-state snapshots
- patch application semantics
- merge/replay/conflict materialization policy
- workspace cleanup, repair, and recovery commands

It does not own:

- run scheduling
- conversation turn state
- approval policy
- provider routing

The goal is not to wrap Git for aesthetic reasons. The goal is to make repo mutation, recovery, and explanation survivable under multi-agent execution.

### 10. Identity and Secrets Plane

This is the single authoritative boundary for identities, credentials, and secret materialization.

It owns:

- operator identity and agent identity records
- provider credential handles and metadata
- extension secret bindings
- secret materialization into runtimes or provider adapters
- provenance for who approved or triggered privileged actions
- rotation, revocation, and invalidation metadata

It does not own:

- authorization decisions
- provider adapter implementations
- workflow scheduling
- repo/worktree lifecycle

The strongest local design stores secret payloads in an OS-backed secret store when available, or an encrypted local vault when it is not, while durable metadata and references live in the persistence substrate.

### 11. Capability and Approval Plane

This is the single authoritative boundary for sensitive permissions and approvals.

It owns:

- tool permission profiles
- worker permission profiles
- extension capability grants
- manual approval and override rules
- approval tokens or grant records for privileged actions
- sensitive-action audit hooks

It does not own:

- conversation turn state
- workflow scheduling
- persistence schemas
- provider adapter implementations

The goal is not merely “more policy code.” The goal is one place where privileged actions become explainable, testable, and durable across all surfaces, using identities and secret handles supplied by the identity plane.

### 12. Provider Adapter Layer

This subsystem is adapter-first, but it is not a peer control plane.

It owns:

- provider protocol implementations
- auth and onboarding metadata
- provider metadata and capability descriptors consumed by the inference plane
- adapter-specific compliance and conformance shims

Provider routing and normalized model behavior should not leak back into surfaces or engines as ad hoc logic.

### 13. Extension Runtime Plane

This subsystem owns executable extension behavior and sandboxing:

- extension catalog and build/import pipeline
- extension runtime host
- hostcall dispatcher and capability policy

It is distinct from instructions because executable extension bundles are a trust and sandbox problem, not just a content problem.

Recommended decomposition:

- `ExtensionBundlePipeline`
- `ExtensionRuntimeHost`
- `HostcallPolicyEngine`

### 14. Instruction Catalog

This subsystem owns declarative instruction assets:

- skill definitions
- prompt bundles
- reusable instruction packs
- typed continuity metadata needed for compaction and resume

It is separate from the extension runtime because declarative instructions should not inherit executable-plugin trust assumptions.

## Data Architecture

### Systems of record

#### Session Event Store

Authoritative for:

- user and assistant turns
- model changes
- compaction summaries
- branch metadata
- skill activation events
- continuity state needed for resume

#### Workflow Store

Authoritative for:

- tasks and runs
- prerequisites and DAG structure
- leases and retries
- worktree bindings
- current verification and completion state
- command idempotency / dedupe records

This store owns the current workflow control truth.

#### Workflow Transition Journal

Authoritative for:

- immutable audit of workflow state transitions
- blockers and handoffs
- worker launches
- verification outcomes

This journal explains how workflow state changed. It is not the hot-path source of current control truth.

#### Workspace Registry

Authoritative for:

- known repos and workspace roots
- live worktree records
- branch/ref associations used by the product
- current workspace ownership and lease bindings
- current merge/replay/conflict state

#### Workspace Journal

Authoritative for:

- immutable workspace lifecycle history
- patch apply attempts
- merge/replay attempts and outcomes
- repair and cleanup actions

#### Execution Ledger

Authoritative for:

- command execution receipts
- file operation receipts
- network/tool execution receipts
- streamed output references
- execution failure classification and timeout/cancel outcomes

#### Inference Ledger

Authoritative for:

- normalized inference request receipts
- model/profile selection and routing decisions
- provider response classification
- token usage and context-fit accounting
- streamed inference output references
- inference retry and failover history

#### Context Pack Ledger

Authoritative for:

- context-pack identities
- source refs and provenance used for context assembly
- retrieval-policy or query digests
- freshness stamps and staleness markers
- usage bindings to turns, runs, compactions, and verification actions

#### Identity Registry

Authoritative for:

- operator identities
- agent identities
- provider credential handles and metadata
- extension secret bindings
- approval actor references
- credential rotation and revocation metadata

#### Resource Registry

Authoritative for:

- resource classes and queue classes
- local concurrency budgets
- provider quota state and token-budget policies
- active reservations and admission ownership
- current saturation markers used for backpressure

#### Admission Journal

Authoritative for:

- admission grants
- admission denials and deferrals
- throttling and cooldown decisions
- reservation release history
- saturation and load-shed events

#### Approval Ledger

Authoritative for:

- approval requests
- approval decisions
- grant issuance and revocation
- manual override authorization records
- security-relevant audit entries

#### Artifact Store

Authoritative for:

- evidence blobs
- verification output
- run artifacts
- extension bundle artifacts

### Physical persistence recommendation

The best physical deployment model is:

- embedded SQLite WAL-backed session event tables
- embedded SQLite WAL-backed workflow current-state tables
- embedded SQLite WAL-backed workflow transition journal tables
- embedded SQLite WAL-backed workspace registry and workspace journal tables
- embedded SQLite WAL-backed execution ledger and execution audit tables
- embedded SQLite WAL-backed inference ledger and inference audit tables
- embedded SQLite WAL-backed context-pack ledger tables plus rebuildable retrieval indexes
- embedded SQLite WAL-backed identity registry tables
- embedded SQLite WAL-backed resource registry and admission journal tables
- OS-backed secret store or encrypted local vault for secret payloads
- embedded SQLite WAL-backed approval and security audit tables
- content-addressed artifact storage
- rebuildable projections/indexes

This is stronger than:

- JSONL primary persistence
- co-equal direct `.sqlite` session files
- V2 as a permanent sidecar
- file-log-only coordination for workflow state

The preferred physical shape is one embedded SQLite substrate with:

- separate session event tables
- separate workflow current-state tables
- separate workflow audit/journal tables
- separate workspace registry and workspace journal tables
- separate execution ledger and execution audit tables
- separate inference ledger and inference audit tables
- separate context-pack ledger tables and retrieval indexes
- separate identity registry tables and secret-handle metadata
- separate resource registry tables and admission journal tables
- separate approval/security audit tables
- separate snapshot/projection tables
- artifact references into CAS storage

This keeps deployment simple without sacrificing authority boundaries.

### Schema and evolution model

Use versioned session event envelopes with:

- stream/store id
- sequence number
- schema id
- timestamp
- correlation and causation ids
- payload
- optional artifact refs

Rules:

- authoritative stores are append-only
- workflow current-state tables are versioned, normalized, and transactionally updated
- workflow state mutations append journal records in the same transaction as current-state changes
- workspace ownership and lifecycle tables are versioned, normalized, and transactionally updated
- execution receipt tables are transactionally updated with immutable audit rows for host actions
- inference receipt tables are transactionally updated with immutable audit rows for model calls
- context-pack tables are transactionally updated with provenance and freshness metadata while indexes remain rebuildable
- identity metadata tables are transactionally updated while secret payloads are materialized via referenced secure storage handles
- resource and quota tables are transactionally updated while admission decisions append immutable journal rows
- approval and grant records are durable, immutable for audit, and referenceable from workflow or extension executions
- projections are disposable and rebuildable
- schema evolution uses upcasters/read adapters for evented stores and explicit migrations for current-state tables
- snapshots are maintained for hot reads but are not sources of truth

### Transaction and consistency rules

- session store writes are atomic per appended event or sealed batch
- workflow writes are atomic per command/transition batch and update current-state tables plus journal together
- workspace writes are atomic per mutation/reconciliation batch and update current-state tables plus journal together
- execution writes are atomic per host-action batch and record both current receipt state and append-only audit details
- inference writes are atomic per inference-attempt batch and record request metadata, token usage, outcome classification, and audit history
- context-pack writes are atomic per assembly/use batch and record provenance before the pack is referenced by turns, launches, or verification actions
- identity metadata writes are atomic per credential or identity change batch, with secret-store handle registration captured durably
- admission writes are atomic per admission/release/rebudget batch and update quota state plus journal together
- approval/grant writes are atomic per decision batch
- artifact refs are written only after the artifact is durable
- cross-store coordination uses IDs and ordering, not distributed transactions
- workflow commands must be idempotent at the persistence boundary
- lease, retry, and worktree ownership updates must be fenced by transactionally checked versions or equivalent guards

Critical invariants:

- one authoritative session history
- one authoritative workflow control state model
- workflow current state is read from control tables, not reconstructed from replaying the journal on hot paths
- one authoritative execution receipt model for host actions
- one authoritative inference receipt and token-accounting model for model calls
- one authoritative context-pack provenance model for retrieved context
- one active lease per task instance
- one active worktree binding owner per leased execution slot
- one authoritative identity and secret-handle model
- one authoritative admission and quota model
- one authoritative workspace owner per repo/worktree mutation path
- one canonical completion decision path
- one canonical approval path for privileged actions
- no caller bypasses resource admission when policy requires it
- turns, runs, and verification actions can explain which context pack they used without reconstructing it from prompt text
- one idempotent processing record per externally retried workflow command
- no projection becomes a co-equal source of truth
- workflow execution only crosses into worker runtimes through typed interfaces
- skill continuity survives compaction through typed state, not summary luck

## Subsystem Disposition

| Subsystem | Verdict | Reason |
|----------|---------|--------|
| CLI/TUI/print surfaces | Keep with narrow modification | Valid product surfaces; must become thin adapters |
| RPC transport | Replace | Transport must stop being a second application core |
| SDK | Keep with narrow modification | Valid surface, but must call the same services as other surfaces |
| Provider trait and core provider types | Keep near-as-is | One of the strongest current boundaries |
| Provider metadata catalog | Keep with narrow modification | Substantively correct; should become more authoritative inside the inference plane and provider adapter layer |
| Provider routing/factory layer | Replace | Routing/failover and capability normalization belong in the inference plane, not in ad hoc factories |
| `src/agent.rs` conversation engine blob | Replace | Correct role, wrong boundary; currently overloaded |
| JSONL primary session persistence | Replace | Not a defensible end-state source of truth |
| direct SQLite session runtime path | Delete after migration | Adds no strong end-state value |
| V2 machinery direction | Keep principle, replace operating shape | Good event-store direction, but not as permanent sidecar |
| session index | Keep as derived projection only | Good derived capability, not source of truth |
| `state`, `reliability`, `task_graph`, RPC workflow logic | Merge and replace | One workflow engine should own this domain |
| orchestration worktree subsystem | Split and replace | Real substrate, but it should become the nucleus of the workspace plane rather than stay buried under workflow |
| built-in tool execution paths | Split and replace | Tool selection belongs higher up, but shell/file/network/process execution should be unified under a host execution plane |
| provider credential handling | Split and replace | Adapter auth requirements belong in the provider adapter layer, but credential ownership and materialization should move to an identity/secrets plane |
| extension runtime manager | Replace | Current production shape is transitional, in-process, and over-privileged |
| extension preflight/scoring/policy | Keep with narrow modification | Useful subsystems under a clearer plane |
| hostcall dispatcher | Keep with narrow modification | Typed dispatcher is correct; surrounding duplication is not |
| skills loader/schema | Keep with narrow modification | File-based skills are right; they belong under a separate instruction catalog and typed activation continuity is missing |
| resources loader | Keep with narrow modification | Discovery layer is useful but should not collapse domains together |
| ad hoc context packing/retrieval heuristics | Merge and replace | One context and retrieval plane should own this domain |
| ad hoc model routing/capability/token-accounting logic | Merge and replace | One inference plane should own this domain |
| ad hoc throttling/quota/backpressure logic | Merge and replace | One resource and admission plane should own this domain |
| scattered tool/worker/extension approval logic | Merge and replace | One capability and approval plane should own this domain |
| ad hoc repo/worktree mutation paths | Merge and replace | One workspace plane should own this domain |
| ad hoc shell/file/network/process execution paths | Merge and replace | One host execution plane should own this domain |
| ad hoc identity/credential/env-secret handling | Merge and replace | One identity and secrets plane should own this domain |

## Stronger End-State Specific Requirements

### Skill continuity

The final architecture must persist:

- skill activation events
- active skill set at compaction boundaries
- enough typed continuity state to restore skill context after resume

Prompt-only continuity is not sufficient.

### Runtime interface discipline

The final architecture must not allow:

- Workflow Engine calling conversation internals ad hoc
- surfaces constructing direct engine-internal state
- provider or extension routing leaking through transport conditionals

Typed service and runtime interfaces are required.

### Unified capability boundary

The final architecture must not allow:

- built-in tools enforcing one permission model while extensions enforce another
- workflow worker launches bypassing the same approval system used by interactive execution
- provider credential access rules living only in provider-specific code
- manual overrides without durable approval records

One capability and approval system is required.

### Workspace correctness boundary

The final architecture must not allow:

- repo mutation logic split across workflow code, built-in tools, and orchestration helpers with no common ownership
- worktree lifecycle truth living only in Git admin metadata or ephemeral process memory
- merge/replay/conflict semantics being different depending on which subsystem triggered them

One workspace plane is required.

### Host execution boundary

The final architecture must not allow:

- built-in tools, worker runtimes, and extensions to each invent their own shell/process semantics
- file/network/process execution receipts to disappear into logs with no durable record
- timeout, cancellation, and execution-environment enforcement to vary by caller instead of by one execution system after common admission

One host execution plane is required.

### Identity and secrets boundary

The final architecture must not allow:

- provider keys, extension secrets, and approval actor identity to leak through ad hoc env vars or scattered config
- authorization logic to also own secret payload storage and materialization
- provider adapters or extension runtimes to become the de facto owners of credential state

One identity and secrets plane is required.

### Resource and admission boundary

The final architecture must not allow:

- provider adapters to invent their own durable rate-limit or cooldown state
- workflow scheduling to bypass one shared fairness and queueing policy
- interactive, workflow, verification, and maintenance work to contend without one admission owner
- saturation decisions to disappear into retry logic, transport loops, or ad hoc caller throttles

One resource and admission plane is required.

### Inference boundary

The final architecture must not allow:

- provider adapters to define normalized model behavior for the product
- conversation or workflow code to branch directly on vendor-specific stream or tool-call quirks
- token-accounting and context-fit policy to be recomputed ad hoc in engines
- routing and failover policy to disappear into provider factories or retry helpers

One inference plane is required.

### Context and retrieval boundary

The final architecture must not allow:

- prompt assembly to become the de facto retrieval system
- worker launches or verifier paths to build opaque context bundles with no durable provenance
- code/session/artifact lookup policy to vary ad hoc by engine or tool path
- context freshness and staleness rules to disappear into prompt heuristics or compaction summaries

One context and retrieval plane is required.

### Extension runtime replacement

The end-state is incomplete if raw JS compatibility runtime remains the production default.

Desired end-state:

- typed out-of-process production runtime
- bundle/import pipeline
- compatibility runtime limited to build/test/migration duties
- direct raw JS execution absent from the production default path

### Provider and instruction placement

The final architecture must not over-elevate support subsystems into peer control planes.

Required placement:

- provider logic is an integration layer serving the engines
- instruction assets live in a declarative catalog
- executable extensions live in a separate runtime plane

### Operator tooling

The final architecture must include:

- store inspectors
- projection rebuild tools
- migration verifiers
- extension bundle diagnostics
- workflow explain/recovery commands
- store-aware `doctor`

## Rejected Alternatives

### 1. Narrow `rpc.rs` extraction only

Rejected because it improves module boundaries without fixing real authority duplication.

### 2. Single monolithic control-plane module

Rejected because it creates a new central choke point and collapses interactive conversation semantics with workflow semantics.

### 3. Single generic event table

Rejected because unrelated domains end up sharing one weakly-typed dumping ground, making invariants harder to enforce and migrations harder to reason about.

### 4. Pure workflow event sourcing

Rejected because leases, retries, worktree ownership, and recovery need canonical transactional current state. Replay-first workflow control adds accidental complexity on the hot path.

### 5. Preserve hybrid session persistence indefinitely

Rejected because co-equal session stores guarantee permanent reconciliation complexity.

### 6. Keep raw JS compatibility runtime as a defining production path

Rejected because compatibility logic would continue to dominate the extension platform.

### 7. Scatter permission and approval policy across subsystems

Rejected because it weakens security, observability, and consistency across CLI/TUI/RPC/SDK and across built-in tools, workers, and extensions.

### 8. Treat worktree and repo state as workflow plumbing

Rejected because repository mutation and recovery are first-class product concerns with their own correctness and operability risks.

### 9. Treat host execution as tool implementation detail

Rejected because shell/file/network/process execution is a first-class product boundary with shared security, cancellation, audit, and failure-mode requirements.

### 10. Treat identity and secret handling as provider or extension detail

Rejected because operator identity, credential materialization, and approval provenance are cross-cutting security and operability concerns.

### 11. Treat admission and saturation as workflow, provider, or host-execution detail

Rejected because local capacity, provider quotas, token budgets, fairness, and backpressure are cross-cutting operability concerns that otherwise turn into duplicated throttles and invisible overload behavior.

### 12. Treat inference semantics as provider adapter detail

Rejected because model capability normalization, routing, stream/tool-call normalization, and token accounting are core product behavior, not incidental vendor plumbing.

### 13. Treat context selection as prompt-assembly or tool-loop detail

Rejected because retrieval policy, context-pack provenance, and freshness control are core product behavior for a coding agent, not incidental prompt scaffolding.

## Bounded Cutover Strategy

This target is only valid if reachable without indefinite coexistence.

Bounded cutover model:

1. define new authoritative contracts and thin surfaces
2. define `WorkerRuntime`, `WorkspaceService`, `HostExecutionService`, `IdentityService`, `CapabilityService`, `ResourceAdmissionService`, `InferenceService`, `ContextService`, and application service interfaces
3. build Persistence Plane and authoritative stores
4. build the unified identity and secrets boundary
5. build the unified capability and approval boundary
6. build the unified resource and admission boundary
7. build the unified inference boundary
8. build the unified host execution boundary
9. build the unified workspace boundary
10. build the unified context and retrieval boundary
11. cut over Conversation Engine to authoritative session events
12. cut over Workflow Engine to authoritative transactional workflow state and merge duplicated task state
13. split provider adapters, extension runtime, and instruction catalog into correct supporting subsystems
14. cut over extension runtime to typed production path
15. add typed skill continuity through compaction/resume
16. switch defaults
17. delete transitional paths
18. re-verify after deletion

Temporary shadowing is allowed only to compare old and new behavior during cutover. It is not an end-state feature.

## End-State Proof Standard

This project is complete only when:

- surfaces are thin
- one Conversation Engine owns interactive session semantics
- one Workflow Engine owns automation/task semantics
- one Inference Plane owns normalized model execution, routing, and token accounting
- one Context and Retrieval Plane owns retrieval policy, context-pack assembly, and provenance
- one Resource and Admission Plane owns admission, queueing, quotas, and backpressure
- one Host Execution Plane owns shell/file/network/process semantics and receipts
- one Workspace Plane owns repo/worktree mutation and recovery semantics
- one Identity and Secrets Plane owns identity, credential handles, and secret materialization
- one Persistence Plane owns authoritative stores and projections
- one Capability and Approval Plane owns privileged-action policy across tools, workers, extensions, and provider access
- provider quirks and model capability logic no longer materially live inside engines or provider factories
- context assembly and provenance no longer materially live inside prompt code, tools, or worker launch helpers
- local capacity limits and provider quotas no longer materially live inside callers, providers, or retry loops
- domain-specific logs and snapshots exist instead of a generic catch-all event table
- workflow control is transactionally authoritative and not replay-first
- raw JS compatibility runtime is no longer the production default
- skill activation continuity survives compaction and resume structurally
- provider adapters and instruction assets are not masquerading as peer control planes
- legacy hybrid storage and duplicate workflow models no longer materially define runtime behavior
