# Environment

Environment variables, external dependencies, and setup notes.

**What belongs here:** Required env vars, external API keys/services, toolchain quirks, platform-specific notes.
**What does NOT belong here:** Service ports/commands (use `.factory/services.yaml`).

---

## Current mission environment status

- Repository root: `/Users/gwizz/CascadeProjects/pi_agent_rust`
- Rust tools exist under `/Users/gwizz/.cargo/bin`
- Homebrew rustup exists under `/opt/homebrew/bin/rustup`
- Current shell sessions may not include Cargo on `PATH`
- Validation was previously blocked by linker/toolchain configuration outside the repo
- Mission execution should assume environment repair may still be required before heavy validation passes

## Tool path guidance

Use absolute tool paths where practical for this mission:
- Cargo: `/Users/gwizz/.cargo/bin/cargo`
- Rustup: `/opt/homebrew/bin/rustup`

## External dependencies

- No new long-running services are planned by default for this mission.
- tmux is expected for TUI smoke validation.
- The repo historically prefers `rch exec -- ...` for heavy compile/test runs, but current availability must be checked in-session.
