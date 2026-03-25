# User Testing

Testing surfaces, validator setup notes, and concurrency guidance.

**What belongs here:** Validation surfaces, testing tools, setup requirements, concurrency limits, environment blockers.
**What does NOT belong here:** General architecture notes.

---

## Validation Surface

Approved user-surface validation for this mission:
- CLI smoke
- RPC smoke
- tmux-backed TUI smoke

Representative flows:
- CLI: help/basic startup and representative non-interactive command routes
- RPC: documented JSONL stdin/stdout request/response flows
- TUI: deterministic tmux-backed interaction checks for help/model/resume/compact/interrupt flows

Use the manifest commands in `.factory/services.yaml` for these surfaces:
- `cli_smoke`
- `rpc_smoke`
- `tui_smoke`

Do not substitute ad hoc raw commands if the manifest command already exists.

## Validation Concurrency

Approved maxima:
- CLI: 1
- RPC: 1
- TUI: 1
- unit/VCR lanes: 2
- full verify lane: 1

Rationale:
- This repo is large and validation is compile-heavy.
- Surface validation should avoid duplicate heavy build pressure.
- TUI/tmux validation is sensitive to noise and should remain single-threaded.

## Current blocker

Validation is not fully runnable yet in the current environment:
- Some shells do not expose Cargo on `PATH`
- Prior dry run found linker/toolchain issues outside the repo
- Use absolute tool paths from `.factory/services.yaml` and `.factory/init.sh`
- If validation still fails due to external environment state, return to orchestrator instead of guessing

## Runtime Hygiene

- Run only the smoke surface required by the scoped feature unless the handoff explicitly requires broader coverage.
- Keep compile-heavy validation serialized where possible to avoid duplicate build pressure.
- After two failed environment/toolchain attempts with the same root cause, stop and hand off instead of trying more command variants.
- If tmux-backed TUI smoke cannot start cleanly from the approved command path, treat that as a blocker and return to orchestrator.
