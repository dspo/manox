#!/usr/bin/env bash
# Build the manox napi core + package the VS Code extension into a vsix.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# 1. Build the napi core (cdylib) and stage it next to the compiled extension.
cargo build -p manox-napi
LIB=""
for candidate in target/debug/libmanox_napi.dylib target/debug/libmanox_napi.so target/debug/manox_napi.dll; do
  if [ -f "$candidate" ]; then
    LIB="$candidate"
    break
  fi
done
[ -n "$LIB" ] || { echo "napi cdylib not found"; exit 1; }
cp "$LIB" vscode/manox_napi.node

# 2. Compile the host TypeScript and bundle the webview frontend.
cd vscode
npx tsc -p ./
npx tsc -p tsconfig.webview.json
node esbuild.webview.mjs
npx tailwindcss -i src/sidebar/webview/styles/tokens.css -o dist/webview/bundle.css --minify

# 3. Package a local vsix (no network deps — the .node and bundles are packaged).
npx vsce package --no-dependencies --out manox-vscode.vsix
echo "vsix built: $(pwd)/manox-vscode.vsix"
