#!/usr/bin/env bash
# Unified offline example gate: every example that exercises a full public
# path must run end to end. Chat examples that need credentials are compile-
# checked only (cargo check -p pi --examples covers them).
set -euo pipefail

cd "$(dirname "$0")/.."

for example in \
  agent_loop_tool \
  coding_agent_smoke \
  compact_run \
  navigate_tree \
  runtime_switch \
  session_resume \
  session_roundtrip \
  split_turn_compact; do
  echo "== $example =="
  cargo run -p pi --example "$example"
done

echo "OK: all offline examples ran"
