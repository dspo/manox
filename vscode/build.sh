#!/usr/bin/env bash
# Build the manox napi core + package the VS Code extension into a vsix.
#
# Usage: vscode/build.sh [options]
# Options:
#   --release   Build the napi binding in release mode (default: debug)
#   -h          Display this help and exit
set -euo pipefail

build_args=()
profile_dir="debug"

help_info() {
    echo "
Usage: ${0##*/} [options]
Build the manox napi core + package the VS Code extension into a vsix.

Options:
  --release   Build the napi binding in release mode (default: debug)
  -h          Display this help and exit
"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) build_args+=(--release); profile_dir="release";;
        -h|--help) help_info; exit 0;;
        *) echo "Error: unknown option '$1'"; help_info; exit 1;;
    esac
    shift
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# 1. Build the napi core (cdylib) and stage it next to the compiled extension.
cargo build ${build_args[@]+"${build_args[@]}"} -p manox-napi
LIB=""
for candidate in target/${profile_dir}/libmanox_napi.dylib target/${profile_dir}/libmanox_napi.so target/${profile_dir}/manox_napi.dll; do
  if [ -f "$candidate" ]; then
    LIB="$candidate"
    break
  fi
done
[ -n "$LIB" ] || { echo "napi cdylib not found"; exit 1; }
cp "$LIB" vscode/manox_napi.node
# Strip debug info: the unstripped debug cdylib (~0.5GB on Linux) makes vsce's
# secret scanner error out and bloats the packaged vsix. `-S` strips debug
# sections on both GNU and Apple strip; plain strip rejects macOS dylibs.
strip -S vscode/manox_napi.node 2>/dev/null || true

# 2. Compile the host TypeScript and bundle the webview frontend.
cd vscode
npx tsc -p ./
npx tsc -p tsconfig.webview.json
node esbuild.webview.mjs
npx tailwindcss -i src/sidebar/webview/styles/tokens.css -o dist/webview/bundle.css --minify

# 3. Package a local vsix (no network deps — the .node and bundles are packaged).
npx vsce package --no-dependencies --out manox-vscode.vsix
echo "vsix built: $(pwd)/manox-vscode.vsix"
