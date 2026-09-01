#!/usr/bin/env bash
# Regenerate the TS Pi differential fixtures from a local TS Pi checkout.
#
# The capture runs the real TS implementation (imported by absolute path)
# under bun. The temp node_modules shims below only need to evaluate at
# import time; the captured functions never call through them.
#
# Usage: PI_TS_REPO=/path/to/pi ./crates/pi/scripts/refresh_ts_pi_fixtures.sh
set -euo pipefail

REPO="${PI_TS_REPO:?set PI_TS_REPO to a TS Pi checkout}"
SHA=$(git -C "$REPO" rev-parse HEAD)
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FIXDIR="$(cd "$SCRIPT_DIR/../tests/fixtures/session" && pwd)"

if ! command -v bun >/dev/null 2>&1; then
  echo "error: bun is required to run the TS capture" >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Eval-time shims for modules the imported TS sources pull in at load time.
mkdir -p "$TMP/node_modules/@earendil-works/pi-ai" "$TMP/node_modules/typebox" \
  "$TMP/node_modules/yaml" "$TMP/node_modules/ignore" "$TMP/node_modules/diff"

cat > "$TMP/node_modules/@earendil-works/pi-ai/package.json" <<'EOF'
{ "name": "@earendil-works/pi-ai", "type": "module", "main": "index.ts" }
EOF
cat > "$TMP/node_modules/@earendil-works/pi-ai/index.ts" <<EOF
export * from "$REPO/packages/ai/src/index.ts";
EOF

cat > "$TMP/node_modules/typebox/package.json" <<'EOF'
{ "name": "typebox", "type": "module", "main": "index.mjs" }
EOF
cat > "$TMP/node_modules/typebox/index.mjs" <<'EOF'
export const Type = new Proxy({}, { get: () => () => ({}) });
export const Static = undefined;
EOF

cat > "$TMP/node_modules/yaml/package.json" <<'EOF'
{ "name": "yaml", "type": "module", "main": "index.mjs" }
EOF
cat > "$TMP/node_modules/yaml/index.mjs" <<'EOF'
export const parse = (s) => JSON.parse(s);
EOF

cat > "$TMP/node_modules/ignore/package.json" <<'EOF'
{ "name": "ignore", "type": "module", "main": "index.mjs" }
EOF
cat > "$TMP/node_modules/ignore/index.mjs" <<'EOF'
export default function ignore() { return { add: () => {}, ignores: () => false }; }
EOF

cat > "$TMP/node_modules/diff/package.json" <<'EOF'
{ "name": "diff", "type": "module", "main": "index.mjs" }
EOF
cat > "$TMP/node_modules/diff/index.mjs" <<'EOF'
export const diffLines = () => [{ added: false, removed: false, value: "" }];
EOF

cp "$SCRIPT_DIR/capture-fixtures.ts" "$TMP/capture-fixtures.ts"
(cd "$TMP" && bun capture-fixtures.ts "$REPO" "$FIXDIR")

cat > "$FIXDIR/README.md" <<EOF
# TS Pi fixtures

Stable artifacts captured from the TypeScript Pi implementation, used by
Rust differential tests. Each fixture records the TS SHA it was generated
from; the Rust tests consume only committed fixtures.

Baseline: \`$SHA\`

Refresh: run \`scripts/refresh_ts_pi_fixtures.sh\` with \`PI_TS_REPO\` pointing at
a checkout of the TS Pi monorepo, then commit the updated fixtures. The
script is a manual, reviewed step; CI only reads what is committed.
EOF

echo "fixtures regenerated from TS Pi at $SHA"
