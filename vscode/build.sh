#!/usr/bin/env bash
# Build the manox napi core + package the VS Code extension into a vsix.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# 1. Build the napi core (cdylib) and stage it next to the compiled extension.
cargo build -p manox-napi
LIB="$(ls target/debug/libmanox_napi.dylib target/debug/libmanox_napi.so target/debug/manox_napi.dll 2>/dev/null | head -1)"
[ -n "$LIB" ] || { echo "napi cdylib not found"; exit 1; }
cp "$LIB" vscode/manox_napi.node

# 2. Compile TypeScript.
cd vscode
npx tsc -p ./

# 3. Package a local vsix (no network deps — the .node is bundled).
npx vsce package --no-dependencies --out manox-vscode.vsix
echo "vsix built: $(pwd)/manox-vscode.vsix"
