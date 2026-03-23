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
