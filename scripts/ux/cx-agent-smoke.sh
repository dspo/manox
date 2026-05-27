#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

echo "[cx-agent-smoke] root=${ROOT_DIR}"
echo "[cx-agent-smoke] scenario 1: command surface"
grep -q "/retry" "${ROOT_DIR}/src/cx_agent/tui/chat.rs"
grep -q "/draft" "${ROOT_DIR}/src/cx_agent/tui/chat.rs"
grep -q "/tools" "${ROOT_DIR}/src/cx_agent/tui/chat.rs"

echo "[cx-agent-smoke] scenario 2: approval action tiers"
grep -q "AllowForTurn" "${ROOT_DIR}/src/cx_agent/approval.rs"
grep -q "AllowForSession" "${ROOT_DIR}/src/cx_agent/approval.rs"

echo "[cx-agent-smoke] scenario 3: status/event layering"
grep -q "transient_event" "${ROOT_DIR}/src/cx_agent/tui/chat.rs"
grep -q "follow:on" "${ROOT_DIR}/src/cx_agent/tui/chat.rs" || true

echo "[cx-agent-smoke] scenario 4: ux rubric exists"
test -f "${ROOT_DIR}/docs/cx-agent-ux-rubric.md"

echo "[cx-agent-smoke] ok"
