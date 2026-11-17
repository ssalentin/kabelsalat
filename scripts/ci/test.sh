#!/usr/bin/env bash
# Unit suite, gated per AGENTS.md rule 50: green only when cargo exits 0 AND
# its output contains "test result: ok" (never a bare grep for "test result").
set -uo pipefail
log=$(mktemp)
cargo test --locked --bin kabelsalat "$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
if [ "$status" -ne 0 ]; then
  echo "cargo test exited $status" >&2
  exit 1
fi
if ! grep -q '^test result: ok\.' "$log"; then
  echo 'cargo test output has no "test result: ok."' >&2
  exit 1
fi
