#!/usr/bin/env bash
# Regenerate the TS Pi fixtures from a local checkout (manual, reviewed).
# Usage: PI_TS_REPO=/path/to/pi ./crates/pi/scripts/refresh_ts_pi_fixtures.sh
set -euo pipefail
REPO="${PI_TS_REPO:?set PI_TS_REPO to a TS Pi checkout}"
SHA=$(git -C "$REPO" rev-parse HEAD)
FIXDIR="$(dirname "$0")/../tests/fixtures/ts-pi"
echo "baseline: $SHA" > "$FIXDIR/README.md"
echo "TODO: regenerate each fixture from $REPO at $SHA"
