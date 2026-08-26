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
- `../webui/` — shared frontend (React 19 + Tailwind v4), built as its own npm
  package; `webui/build.sh` stages the bundle and the contract modules
  (`protocol.ts`/`messages.ts`) under `vscode/dist/` for the host's imports
  and the packaged vsix
- `../crates/manox-actor`, `../crates/manox-napi` — Rust core (workspace root)

## Develop

```sh
# one-time: webui deps (frontend) + this package's deps
( cd ../webui && npm install ) && npm install
# build the napi binding once (from the repo root), then on every Rust change:
( cd .. && cargo build -p manox-napi )

# rebuild the shared webui (bundle + contract staging into vscode/dist/) and
# compile the host to out/; `npm run compile` restages contracts first
( cd ../webui && npm run build )
npm run compile        # host typecheck, emits to out/
npm test               # vitest: session manager + host bridge tests
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

`build.sh` uses a **debug** napi build for fast local iteration; pass
`--release` (`bash vscode/build.sh --release`) before distributing.

## Protocol

Host ↔ actor wire protocol is typed in `../webui/src/protocol.ts` (mirrored by
`crates/manox-actor/src/{actor,events}.rs`). Host ↔ webview postMessage
protocol lives in `../webui/src/sidebar/messages.ts`. Both are owned by the
`webui/` package and shared with the browser host.
