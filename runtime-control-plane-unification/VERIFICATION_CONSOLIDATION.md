# Verification Consolidation - End-State Design

**Date**: 2026-03-22

## Objective

The verification architecture must answer two questions clearly:

1. who decides whether a workflow step is complete?
2. where does the proof of that decision live?

In the strongest end-state:

- the `Workflow Engine` owns verification semantics and completion decisions
- the `Inference Plane` owns normalized model execution for any verifier path that depends on model calls
- the `Context and Retrieval Plane` owns verifier context-pack assembly and provenance for model-backed verification paths
- the `Identity and Secrets Plane` owns durable identity for approvers and override actors
- the `Capability and Approval Plane` owns who is allowed to override or bypass normal verification
- the `Resource and Admission Plane` owns whether verification execution is admitted now and under which budget class
- the `Workflow Store` owns current verification state and completion bindings
- the `Persistence Plane` owns durable verification audit history, evidence references, and artifacts

Verification is therefore a workflow concern backed by an authoritative persistence model, not a helper namespace.

## Current Problem

Verification meaning is split across:

- `src/reliability/verifier.rs`
- `src/verification/`
- `src/state/gates.rs`
- embedded run report fields in `src/orchestration/run.rs`

That causes:

- scattered completion rules
- duplicated evidence semantics
- weak operator visibility
- no single place to inspect why a task or run was accepted or rejected

## End-State Ownership Model

### Workflow Engine owns

- verification policy
- verification scopes
- completion verdict rules
- acceptance criteria evaluation
- non-privileged completion semantics

### Inference Plane owns

- normalized model execution and token accounting for model-backed verification paths
- provider-neutral verifier inference receipts and failure classification

### Context and Retrieval Plane owns

- evidence-context pack assembly for model-backed verification
- provenance and freshness tracking for retrieved verification context

### Capability and Approval Plane owns

- privileged override authorization
- manual approval requirements for verification bypass
- policy over which identities may approve privileged completion paths
- durable identity of who approved a privileged completion path

### Resource and Admission Plane owns

- admission of verification executions, verifier worker launches, and expensive verification retries
- fairness and queueing policy between verification work and other admitted workloads
- durable saturation and deferral history for verification execution

### Identity and Secrets Plane owns

- operator and agent identity records referenced by verification approvals
- durable actor identity bindings for override and approval provenance

### Persistence Plane owns

- workflow-linked verification state
- verification audit/journal history
- evidence references
- artifact durability
- rebuildable verification projections and snapshots

This split is stronger than a generic `verification` module because it aligns control ownership with data ownership.

## Verification Data Model

Logical authoritative records should include:

- `verification_requirements`
- `verification_runs`
- `verification_decisions`
- `verification_overrides`
- `verification_override_approvals`
- `verification_actor_refs`
- `verification_context_pack_refs`
- `verification_scopes`
- `evidence_refs`
- `acceptance_refs`
- `artifacts`
- `verification_journal`

Recommended journal kinds:

- verification requested
- verification executed
- evidence attached
- acceptance mapped
- verification context packed
- completion accepted
- completion rejected
- operator override recorded
- override approved
- approver identity recorded

The physical substrate should be:

- workflow verification tables for current decision state
- append-only verification journal rows for audit history
- artifact CAS for immutable evidence payloads

Verification should use domain-specific workflow tables, audit journals, and projection tables, not a generic cross-domain event bucket.

## Verification Scopes

Permanent scopes should include:

- `Task`
- `Run`
- `Handoff`
- `SessionClose`

Wave-level verification can exist if the Workflow Engine materially needs it, but it should not be forced into the permanent design unless it affects real decisions.

## Completion Invariants

The strongest end-state should enforce:

- a task instance cannot enter succeeded terminal state without successful verification or an explicit override event
- an override cannot take effect without an approval record when policy requires one
- privileged approvals are attributable to durable actor identity records
- model-backed verification can explain which context pack it evaluated
- verification execution deferrals and throttles remain durable history, not transient queue state
- verification failures remain durable history, not transient logs
- current verification truth is queryable without replaying full audit history
- acceptance criteria are not considered satisfied unless mapped to evidence
- run/session projections derive from verification history, not prompt narrative

## API Shape

The right API split is:

### Workflow-facing commands

- `request_verification(...)`
- `record_verification_result(...)`
- `finalize_task_completion(...)`
- `record_override(...)`
- `record_override_approval(...)`
- `bind_override_actor(...)`

### Persistence-facing writes

- update workflow verification state
- append verification journal record
- attach evidence ref
- attach artifact ref
- rebuild verification projections and snapshots

### Query projections

- `task_verification_summary(...)`
- `run_verification_summary(...)`
- `session_close_summary(...)`

## What To Adopt From Current Code

Adopt:

- scope audit logic from `src/reliability/verifier.rs`
- evidence discipline from `src/verification/`
- the existing cultural rule that unverifiable claims should be blocked

Modify:

- move decision logic into the Workflow Engine
- route actor identity provenance through the Identity and Secrets Plane
- route privileged override authorization through the Capability and Approval Plane
- turn helper outputs into durable workflow records plus audit-journal entries
- make projections and snapshots rebuildable from authoritative verification history without making replay the hot-path read model

Reject:

- separate live verification owners
- gate infrastructure as a co-equal completion authority
- verifier-local throttling or queue state outside the resource and admission plane
- task/run completion logic embedded primarily in transport or ad hoc report fields

## Deletion Plan

After the Workflow Engine and Persistence Plane own verification:

1. migrate all callers to workflow verification commands
2. stop reading production completion truth from `src/reliability/verifier.rs`
3. stop reading production completion truth from `src/verification/`
4. stop reading production completion truth from `src/state/gates.rs`
5. delete or quarantine legacy modules once projections and runtime behavior no longer depend on them

## Completion Standard

Verification consolidation is complete only when:

- one engine owns verification semantics
- one persistence model owns verification state, history, and evidence refs
- verification context provenance is durable and inspectable
- verification execution admission and deferral are durable and inspectable
- privileged approvals are attributable to durable identity records
- privileged overrides are policy-controlled and durably approved
- completion decisions derive from durable verification records and evidence refs
- surfaces consume derived verification summaries
- old verification modules no longer materially define runtime completion behavior
