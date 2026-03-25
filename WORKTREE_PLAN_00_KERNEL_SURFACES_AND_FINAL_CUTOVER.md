# Worktree Plan 00: Kernel, Surfaces, Engine Integration, and Final Cutover

Worktree: `/Users/gwizz/CascadeProjects/pi_agent_rust`  
Status: `in_progress`  
Role: serial surface demotion first, then final integration, default cutover, and deletion

## Why This Worktree Still Exists

This branch is the serial control point for the whole program. The other worktrees can build subsystem replacements in parallel, but only this worktree may:

- demote the entry surfaces to adapters
- integrate the subsystem branches into one runtime
- switch the default runtime path
- delete the legacy routes that were replaced

The review result is clear: this branch is not near completion. It has real progress, but it is still in pre-cutover surface thinning.

## Current Baseline

Already landed here:

- contract/bootstrap seams
- fail-closed non-interactive extension UI behavior
- route/helper extraction from `src/main.rs`
- startup helper extraction for auth, resources, input prep, and recovery

What that means:

- the branch has useful adapter-thinning progress
- the branch has **not** yet made `src/main.rs` composition-only
- the branch has **not** yet made `src/rpc.rs` transport-only
- the branch has **not** yet integrated the new subsystem authorities

## Owns

- `src/contracts/*`
- `src/surface/*`
- `src/main.rs`
- `src/app.rs`
- `src/interactive.rs`
- `src/rpc.rs`
- `src/sdk/*`
- final integration edits to `src/agent.rs`
- final routing deletion and replacement proof

## Does Not Own

Do not implement subsystem internals here except the minimum boundary extraction needed for cutover:

- persistence/workflow internals
- context/inference/admission internals
- identity/approval/extension-host internals
- host-execution/workspace internals

Those remain owned by Worktrees 01-04.

## Remaining Plan

### Stage 1: Finish Surface Demotion

Required outcome:

- `src/main.rs` becomes composition, surface selection, and adapter orchestration only
- `src/rpc.rs` becomes transport DTOs, protocol translation, and transport lifecycle only
- `src/app.rs` and `src/interactive.rs` stop carrying business-state ownership

Concrete remaining work:

- move startup/session/provider/extension policy out of `src/main.rs`
- move task/run/session authority out of `src/rpc.rs`
- remove parallel business paths across CLI, interactive, and RPC startup flows
- make all surface routes converge on typed kernel/service entrypoints

### Stage 2: Prepare Integration Seams

Required outcome:

- stable engine-facing contracts exist and are actually used by the surfaces
- surface code depends on `ConversationService`, `WorkflowService`, and cross-plane services, not legacy internals

Concrete remaining work:

- introduce the thin kernel/composition layer that owns service assembly
- make surface routing call kernel/service boundaries instead of inline runtime construction
- isolate any remaining bypass paths that would prevent later subsystem cutover

### Stage 3: Integrate Worktrees 01-04

Required outcome:

- one `ConversationEngine` path
- one `WorkflowEngine` path
- all provider, approval, execution, workspace, and persistence behavior reached through the new services

Concrete remaining work:

- merge WT01 first, then WT02 and WT04, then WT03
- rebuild runtime wiring around the real subsystem implementations
- remove legacy `rpc`/`reliability`/`state` ownership from normal routing

### Stage 4: Default Cutover and Deletion

Required outcome:

- the new architecture is the default runtime path
- replaced legacy routes are removed or disabled
- replacement proof is explicit

Deletion targets:

- transport-owned business contracts/state in `rpc`
- legacy workflow authority routes
- legacy session runtime truth routes
- any old default path that bypasses the new services

## Stop Doing

Do not spend more time here on helper extraction unless the commit clearly does one of these:

- removes a real business responsibility from a surface file
- creates a stable kernel/service boundary used by more than one route
- deletes or disables a legacy runtime path

More local cleanup without authority movement does not advance the plan enough.

## Parallel Rule

This worktree has two valid modes:

1. keep demoting the surfaces while touching only worktree-owned files
2. pause for subsystem branches to produce real runtime authorities, then resume for integration/cutover

Do not start engine cutover until Worktrees 01-04 have real call-ready services, not just contracts or receipts.

## Exit Gate

This worktree is only complete when all of the following are true:

- `src/main.rs` is composition/route selection only
- `src/rpc.rs` is transport-only
- interactive and non-interactive surfaces hit the same engine boundaries
- the new engine/service path is the default runtime path
- replaced legacy runtime routes are removed or disabled

## Suggested Commit Slices

1. Move remaining startup/session/provider policy out of `src/main.rs`
2. Remove task/run/session authority from `src/rpc.rs`
3. Install kernel/service assembly as the only surface target
4. Merge and wire WT01
5. Merge and wire WT02 and WT04
6. Merge and wire WT03
7. Switch defaults to the new runtime path
8. Delete legacy routes and capture replacement proof
