# Worker Role Elevation - End-State Design

**Date**: 2026-03-22

## Objective

Workers must become first-class execution roles owned by the `Workflow Engine` and executed through explicit worker runtimes.

The goal is not only better prompts. The goal is:

- durable role policy
- explicit launch records
- predictable context, permission, and admission boundaries
- verifiable expectations for output, handoff, and completion

## Ownership Model

### Workflow Engine owns

- role taxonomy
- role selection policy
- launch decisions
- lease and worktree binding
- completion and verification expectations

### Conversation Engine owns

- execution of conversation-style workers
- turn loop and prompt/tool interaction for those workers

### Inference Plane owns

- model-profile and capability normalization for worker model calls
- routing/failover and token-accounting policy for worker inference
- normalized model-call receipts consumed by worker runtimes

### Context and Retrieval Plane owns

- context selection and retrieval policy for worker launches
- context-pack assembly and provenance for worker executions
- freshness/staleness evaluation for retrieved worker context

### Host Execution Plane owns

- shell/file/network/process execution used by workers
- execution receipt capture and streamed output references
- timeout, cancellation, and execution-environment limit semantics for worker-side host actions after admission

### Workspace Plane owns

- worktree allocation and lifecycle
- repo/ref binding for worker executions
- patch application and merge/replay materialization used by workers

### Identity and Secrets Plane owns

- provider credential handles for worker executions
- extension secret bindings for non-conversation runtimes
- durable actor identity used for privileged worker actions

### Capability and Approval Plane owns

- permission profiles
- approval requirements for privileged worker actions
- extension capability grants for non-conversation runtimes

### Resource and Admission Plane owns

- admission classes for worker launches
- queue priority and fairness policy across worker kinds
- local concurrency budgets and provider-budget gating for worker work
- durable admission grants, deferrals, and saturation signals

### Persistence Plane owns

- durable role definitions or role references
- worker launch records in the workflow store
- worker launch audit history
- role-linked completion and handoff history

This is stronger than treating worker roles as a prompt-building detail.

### Runtime interface rule

The `Workflow Engine` should launch work only through a `WorkerRuntime` interface.

The `Conversation Engine` is one implementation of that interface, not a bag of internals the workflow system is allowed to reach into directly.

## Current Problem

The current weakness is not just that workers use thin prompts. It is that:

- role policy is not durable
- worker launches are not clearly owned by workflow semantics
- launch constraints and expectations are not recorded as authoritative state
- permission, approval, and admission rules are not unified across worker kinds
- context selection and provenance are still too easy to hide in prompt-building code

## Recommended Role Model

Each worker launch should carry:

- `role_id`
- `role_kind`
- `worker_runtime_kind`
- `system_prompt_template`
- `tool_permission_profile`
- `inference_profile`
- `context_profile`
- `required_context_policy`
- `context_budget_policy`
- `execution_constraints`
- `admission_class`
- `verification_policy`
- `handoff_policy`
- `failure_policy`

Recommended initial built-in roles:

- `planner`
- `implementor`
- `reviewer`
- `verifier`

`worker_runtime_kind` matters because not every future worker must be a pure conversation-style runtime.

The target design should explicitly allow:

- `conversation_worker`
- `sandboxed_extension_worker`
- `native_verifier_worker`

## Launch Invariants

Every launch should durably record:

- task instance id
- selected role
- worker runtime kind
- runtime bundle or executable digest
- inference profile id or digest
- context pack ref or digest
- capability profile id
- approval requirement id or policy digest
- admission ticket or admission policy digest
- lease id
- worktree binding
- prompt template/version or digest
- permissions profile
- context inputs
- expected verification policy

This is necessary for explainability, recovery, and postmortem inspection.

## Integration Flow

Recommended flow:

1. Workflow Engine selects the next task instance.
2. Workflow Engine selects the required role.
3. Identity and Secrets Plane resolves the worker's actor identity and secret bindings.
4. Context and Retrieval Plane resolves the worker's context pack and provenance.
5. Inference Plane resolves the worker's model profile and normalized inference contract where the runtime is model-backed.
6. Capability and Approval Plane resolves the worker's effective grants and approval requirements.
7. Resource and Admission Plane resolves admission, queue placement, or deferral.
8. Workspace Plane allocates or renews lease/worktree binding.
9. Workflow Engine emits a worker-launch record into the Persistence Plane.
10. Conversation Engine or other worker runtime executes the task with that role profile, identity binding, context pack, inference profile, capability grant, admission ticket, and workspace binding.
11. Host Execution Plane performs any shell/file/network/process actions required by that runtime.
12. Completion, handoff, and verification results update workflow state and append audit history.

## Role Selection Policy

Role selection should derive from durable task metadata, including:

- task category
- required permissions
- verification intensity
- context breadth
- planning vs implementation vs review vs repair mode

This prevents role policy from becoming a pile of ad hoc prompt-conditionals.

## What To Adopt From Earlier Drafts

Adopt:

- explicit `WorkerRole` type
- role-specific prompt templates
- role-specific permission profiles
- lifecycle hooks where they improve observability

Modify:

- make the Workflow Engine the authority over roles
- make the Conversation Engine only one possible worker runtime
- make context-pack selection explicit instead of letting retrieval leak through prompt defaults or ad hoc tool reads
- make inference profile selection explicit instead of letting model/provider choice leak through prompts or runtime defaults
- make capability and approval policy explicit instead of prompt-derived
- persist launch history as typed records plus audit entries

Reject:

- generic worker sessions with default configuration
- orchestration that assumes every worker is an in-process conversation loop
- role semantics that live only in transient runtime code

## Completion Standard

Worker-role elevation is complete only when:

- workflow launches are role-aware and durable
- role identity and launch context are queryable from persisted state
- permissions, context, and verification expectations are role-derived
- worker launches bind durable context-pack refs through one context and retrieval system
- worker launches are admitted through one resource and admission system
- privileged worker actions are governed by one capability and approval system
- generic default-session worker launches no longer materially define orchestration behavior
