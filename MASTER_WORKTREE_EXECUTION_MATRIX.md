# Master Worktree Execution Matrix

Status: re-baselined execution ledger after plan-vs-implementation review  
Date: 2026-03-24  
Primary repo: `/Users/gwizz/CascadeProjects/pi_agent_rust`

## Why This Matrix Was Rewritten

The original worktree plans were still too close to end-state architecture documents. After review, the real gap is clear:

- the branches have landed useful scaffolding
- most of them have **not** yet replaced the legacy runtime paths they were created to replace

This matrix therefore tracks the **remaining execution program**, not the ideal architecture in abstract.

## Governing Rule

For every worktree:

- verified scaffolding counts as progress
- scaffolding alone does **not** close a replacement item
- a worktree only exits when its owned legacy runtime path loses authority or becomes deletion-ready

## Current Worktree Roster

| Worktree | Plan file | Current state | What is real now | What remains open |
|---|---|---|---|---|
| `WT00` `/Users/gwizz/CascadeProjects/pi_agent_rust` | [WORKTREE_PLAN_00_KERNEL_SURFACES_AND_FINAL_CUTOVER.md](./WORKTREE_PLAN_00_KERNEL_SURFACES_AND_FINAL_CUTOVER.md) | `in_progress` | contract/bootstrap seams, non-interactive fail-closed handling, helper extraction from `main.rs` | `main.rs` composition-only, `rpc.rs` transport-only, engine integration, default cutover, deletion |
| `WT01` `/Users/gwizz/CascadeProjects/pi_agent_rust-architecture-plan-20260324` | [WORKTREE_PLAN_01_PERSISTENCE_AND_WORKFLOW_STATE.md](../pi_agent_rust-architecture-plan-20260324/WORKTREE_PLAN_01_PERSISTENCE_AND_WORKFLOW_STATE.md) | `in_progress` | continuity/journal/resume/todo scaffolding | authoritative session/workflow/planning truth cutover |
| `WT02` `/Users/gwizz/CascadeProjects/pi_agent_rust-endstate-architecture-plan-20260324` | [WORKTREE_PLAN_02_CONTEXT_INFERENCE_AND_ADMISSION.md](../pi_agent_rust-endstate-architecture-plan-20260324/WORKTREE_PLAN_02_CONTEXT_INFERENCE_AND_ADMISSION.md) | `in_progress` | context/inference receipts, continuity capsules, freshness/admission scaffolding | live context/inference/admission service cutover |
| `WT03` `/Users/gwizz/CascadeProjects/pi_agent_rust-endstate-plan-20260324` | [WORKTREE_PLAN_03_IDENTITY_APPROVAL_AND_EXTENSION_HOST.md](../pi_agent_rust-endstate-plan-20260324/WORKTREE_PLAN_03_IDENTITY_APPROVAL_AND_EXTENSION_HOST.md) | `in_progress` | approval/launch/host protocol scaffolding | authoritative identity/approval and out-of-process extension default |
| `WT04` `/Users/gwizz/CascadeProjects/pi_agent_rust-endstate-review` | [WORKTREE_PLAN_04_HOST_EXECUTION_AND_WORKSPACE.md](../pi_agent_rust-endstate-review/WORKTREE_PLAN_04_HOST_EXECUTION_AND_WORKSPACE.md) | `in_progress` | execution/workspace service and receipt/index scaffolding | host/workspace call-site migration and implicit-path replacement |

## Logical Track Compression

The same logical compression still holds:

- `WT00`: kernel/contracts, conversation cutover, workflow cutover, surface cutover, deletion
- `WT01`: persistence plane, workflow/planning authority
- `WT02`: context, inference, admission
- `WT03`: identity, approval, extension runtime
- `WT04`: host execution, workspace

What changed is execution priority: branches now optimize for runtime authority replacement, not more abstract boundary growth.

## Remaining Execution Waves

### Wave A: Finish WT00 Surface Demotion

Owner:

- `WT00`

Goal:

- make the entry surfaces thin enough that subsystem replacement can actually plug in

Required proof:

- `src/main.rs` is on a path to composition-only
- `src/rpc.rs` is on a path to transport-only
- no new surface-local business authority is being introduced

### Wave B: Parallel Runtime Authority Replacement

Owners:

- `WT01`
- `WT02`
- `WT03`
- `WT04`

Goal:

- replace the legacy runtime authorities in each owned subsystem, not just add scaffolding beside them

Required rule:

- each commit should either cut over a real runtime read/write/call path or clearly enable the next cutover
- receipt-only, wrapper-only, or helper-only work is insufficient unless it advances a live replacement boundary

### Wave C: Serial Integration and Default Cutover

Owner:

- `WT00`

Goal:

- integrate WT01-WT04 into one runtime
- switch default routing
- delete or disable replaced legacy paths

Required proof:

- new services are the default runtime path
- old paths no longer materially define normal runtime behavior

## Revised Merge Order

1. WT00 surface demotion boundary
2. WT01 authority cutover
3. WT02 and WT04 runtime service cutovers
4. WT03 default-path cutover once WT04 host boundary is real
5. WT00 engine integration and default routing
6. WT00 legacy deletion and replacement proof

## Phase-to-Worktree Matrix

| Remaining phase | Owning worktree | Hard dependency | Parallel-safe with | Replacement proof required |
|---|---|---|---|---|
| Surface demotion in `main`/`rpc`/`interactive`/`app` | `WT00` | none | none | surfaces stop owning business authority |
| Authoritative session truth | `WT01` | WT00 contracts | WT02, WT03, WT04 | legacy session truth stops being co-equal runtime authority |
| Authoritative workflow/planning truth | `WT01` | WT00 contracts | WT02, WT03, WT04 | `reliability`/`state`/`task_graph` stop being co-equal runtime authority |
| Context service cutover | `WT02` | WT00 contracts, WT01 receipt/store interfaces | WT01, WT03, WT04 | context assembly stops being prompt glue |
| Inference service cutover | `WT02` | WT00 contracts, WT01 receipt/store interfaces | WT01, WT03, WT04 | engine/provider branching no longer owns provider normalization |
| Admission service cutover | `WT02` | WT00 contracts, WT01 receipt/store interfaces | WT01, WT03, WT04 | admission becomes shared durable behavior |
| Identity/grant runtime cutover | `WT03` | WT00 contracts, WT01 durability | WT01, WT02, WT04 | privileged behavior resolves through identity/grant state |
| Approval-policy runtime cutover | `WT03` | WT00 contracts, WT01 durability | WT01, WT02, WT04 | approval becomes unified and restart-safe |
| Extension default-path replacement | `WT03` | WT04 host boundary, WT01 durability | WT01, WT02, WT04 | out-of-process host becomes default runtime |
| Host-execution boundary cutover | `WT04` | WT00 contracts, WT01 receipts, WT02/WT03 hooks | WT01, WT02, WT03 | raw machine side effects stop bypassing the boundary |
| Workspace lifecycle cutover | `WT04` | WT00 contracts, WT01 receipts | WT01, WT02, WT03 | detached worktree authority becomes durable and queryable |
| Engine integration | `WT00` | merged WT01-WT04 | none | conversation/workflow engines use the new subsystem authorities |
| Default routing and deletion | `WT00` | engine integration complete | none | replaced legacy paths are removed or disabled |

## Shared-File Ownership

| File or area | Primary owner | Notes |
|---|---|---|
| `src/contracts/*` | `WT00` | no parallel deep edits |
| `src/main.rs` | `WT00` | serial surface demotion and cutover only |
| `src/app.rs` | `WT00` | serial surface demotion and cutover only |
| `src/interactive.rs` | `WT00` | serial surface demotion and cutover only |
| `src/rpc.rs` | `WT00` | transport demotion and legacy removal only |
| `src/agent.rs` | `WT00` | final engine integration only |
| `src/session*` | `WT01` | authoritative session truth |
| `src/reliability/*`, `src/state/*`, `src/task_graph/*` | `WT01` | workflow/planning authority replacement |
| `src/context/*`, `src/compaction*`, provider/model selector internals | `WT02` | context/inference/admission authority |
| `src/auth/*`, `src/policy/*`, `src/extensions*`, `src/extension_dispatcher.rs` | `WT03` | identity/approval/extension runtime authority |
| `src/tools/*`, `src/sandbox/*`, `src/orchestration/flock.rs` | `WT04` | host execution/workspace authority |

## Exit Gates

| Worktree | Exit gate |
|---|---|
| `WT00` | surfaces are thin, new services are the default runtime path, replaced legacy routes are removed or disabled |
| `WT01` | one authoritative runtime truth exists for session/workflow/planning state |
| `WT02` | one authoritative context/inference/admission service path exists for live model calls |
| `WT03` | one authoritative identity/approval model exists and the out-of-process extension host is default |
| `WT04` | one authoritative host-execution/workspace boundary owns live machine mutation and detached worktree lifecycle |

## Status Ledger

Use the canonical status model from `AGENTS.md`.

| Worktree | Status | Last credible closed boundary | Next required boundary |
|---|---|---|---|
| `WT00` | `in_progress` | helper extraction and fail-closed surface guard hardening | surface demotion with authority movement out of `main` and `rpc` |
| `WT01` | `in_progress` | typed continuity/journal/resume scaffolding | authoritative session/workflow runtime truth |
| `WT02` | `in_progress` | typed context/inference/admission receipts and capsules | live context/inference/admission service cutover |
| `WT03` | `in_progress` | approval/launch/host protocol scaffolding | identity/approval authority and extension default-path cutover |
| `WT04` | `in_progress` | execution/workspace service and receipt/index scaffolding | live host/workspace call-site migration |
