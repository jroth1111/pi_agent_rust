#!/bin/sh
set -eu

REPO_ROOT="/Users/gwizz/CascadeProjects/pi_agent_rust"
CARGO_BIN="/Users/gwizz/.cargo/bin/cargo"
RUSTUP_BIN="/opt/homebrew/bin/rustup"

if [ -x "$RUSTUP_BIN" ]; then
  export PATH="/opt/homebrew/bin:$PATH"
fi
if [ -d "/Users/gwizz/.cargo/bin" ]; then
  export PATH="/Users/gwizz/.cargo/bin:$PATH"
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/Users/gwizz/CascadeProjects/pi_agent_rust/.tmp_cargo_tmp_final}"
mkdir -p "$CARGO_TARGET_DIR"

if [ ! -x "$CARGO_BIN" ]; then
  echo "Expected cargo at $CARGO_BIN but it is not executable." >&2
  exit 1
fi

# Validation remains blocked until the user fixes the external linker/toolchain setup.
# This script only normalizes PATH and stable absolute tool locations for workers.
exit 0
