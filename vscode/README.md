# manox VS Code extension

manox agent workbench inside VS Code: a sidebar webview chat plus the
`@manox` chat participant, backed by the pi kernel running in-process
through the `manox-napi` native binding.

## Layout

- `src/extension.ts` — activation, wiring sidebar + participant
- `src/sessionManager.ts` — session multiplexer over the shared actor transport
- `src/transport/` — `Transport` interface + napi implementation
- `src/participant.ts` — `@manox` chat participant (steers to the sidebar)
- `src/sidebar/` — webview host + postMessage protocol
- `src/sidebar/webview/` — React 19 + Tailwind v4 frontend (bundled separately)
- `../crates/manox-actor`, `../crates/manox-napi` — Rust core (workspace root)

## Develop

```sh
npm install
# build the napi binding once (from the repo root), then on every Rust change:
( cd .. && cargo build -p manox-napi )

npm run compile        # host + webview typecheck, host emits to out/
npm run build:webview  # esbuild bundle + Tailwind CSS into dist/webview/
npm test               # vitest: store reducer + session manager
```

Run the extension in a development host (F5 from the repo root's
`.vscode/launch.json`, or `code --extensionDevelopmentPath=.`).

## Package

```sh
# from the repo root: builds the napi cdylib, compiles, bundles, and
# produces vscode/manox-vscode.vsix
bash vscode/build.sh
code --install-extension vscode/manox-vscode.vsix --force
```

`build.sh` uses a **debug** napi build for fast local iteration; switch it to
`cargo build --release` before distributing.

## Protocol

Host ↔ actor wire protocol is typed in `src/protocol.ts` (mirrored by
`crates/manox-actor/src/{actor,events}.rs`). Host ↔ webview postMessage
protocol lives in `src/sidebar/messages.ts`.
