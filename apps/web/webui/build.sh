#!/usr/bin/env bash
# Build the webui package: webview bundle (esbuild + tailwind) and the
# contract modules for TypeScript hosts (tsc commonjs, staged under
# apps/vscode/dist). The bundle is checked into the repo for the browser host
# (PR3 embeds it with include_dir!); the contract output is a build product.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT/apps/web/webui"

# 1. Webview bundle + staging under apps/vscode/dist (the host loads it from
#    dist/webview/, which vsce packages into the vsix).
node esbuild.mjs
npx tailwindcss -i src/sidebar/webview/styles/tokens.css -o dist/webview/bundle.css --minify
mkdir -p "$ROOT/apps/vscode/dist/webview"
cp -f dist/webview/bundle.js dist/webview/bundle.css "$ROOT/apps/vscode/dist/webview/"

# 2. Contract modules (protocol.ts / messages.ts) + staging under vscode/dist.
npm run build:contracts

echo "webui built: $(pwd)/dist"
