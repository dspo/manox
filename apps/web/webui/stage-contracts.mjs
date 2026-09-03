// Stage the typed protocol contract under apps/vscode/dist so the VS Code
// host's tsc resolves `FromClient` / `FromServer` / `ServerNote` / UI
// projections (ThreadInfoSnapshot, CommandEntry, …) and the webview↔host
// `ToHost` / `ToWebview` shapes. No tsc emit: the contract is type-only, so
// the sources are staged as .d.ts directly.
//
//   vscode/dist/protocol.d.ts        = ts-rs bindings (crates/.../protocol.ts)
//                                      + the webui UI-projection types from
//                                      src/protocol.ts (with its bindings
//                                      re-export line stripped, since the
//                                      bindings are inlined above them).
//   vscode/dist/serde_json/JsonValue.d.ts
//                                    = the JsonValue sibling the bindings
//                                      import.
//   vscode/dist/sidebar/messages.d.ts = src/sidebar/messages.ts verbatim
//                                      (its `../protocol` import resolves to
//                                      the staged protocol.d.ts).
import { cp, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));
const target = path.join(root, '..', '..', 'vscode', 'dist');
const bindingsDir = path.join(root, '..', '..', '..', 'crates', 'manox-protocol', 'bindings');

await mkdir(path.join(target, 'sidebar'), { recursive: true });
await mkdir(path.join(target, 'serde_json'), { recursive: true });

// ── protocol.d.ts: bindings + webui UI projections ────────────────────────
const bindings = (await readFile(path.join(bindingsDir, 'protocol.ts'), 'utf8'))
	.split('\n')
	.filter((l) => !l.startsWith('import type { JsonValue }'))
	.join('\n');

const webuiProtocol = await readFile(path.join(root, 'src', 'protocol.ts'), 'utf8');
// Drop the bindings re-export (the bindings are inlined above); keep only the
// UI-projection types that reference the inlined bindings.
const uiTypes = webuiProtocol
	.split('\n')
	.filter((l) => !l.includes("export * from '") && !l.includes('export * from "'))
	.join('\n');

const jsonValueReExport = "export type { JsonValue } from './serde_json/JsonValue';\n";
const protocolDts = `${jsonValueReExport}\n${bindings}\n\n${uiTypes}\n`;
await writeFile(path.join(target, 'protocol.d.ts'), protocolDts);

// ── serde_json/JsonValue.d.ts ─────────────────────────────────────────────
await cp(
	path.join(bindingsDir, 'serde_json', 'JsonValue.ts'),
	path.join(target, 'serde_json', 'JsonValue.d.ts'),
);

// ── sidebar/messages.d.ts (verbatim; `../protocol` resolves to staged) ────
await cp(
	path.join(root, 'src', 'sidebar', 'messages.ts'),
	path.join(target, 'sidebar', 'messages.d.ts'),
);

console.log('contracts staged into vscode/dist');
