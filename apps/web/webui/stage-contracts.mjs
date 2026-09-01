// Copy the compiled contract modules (protocol/messages) under vscode/dist
// so the host's tsc emits require()s that resolve both in development and
// inside the packaged vsix (vsce packages vscode/dist unconditionally). The
// sources stay owned by webui/.
//
// Also stage the AgentServer protocol types (ts-rs generated bindings from
// crates/manox-protocol) merged into the protocol.d.ts so the vscode host
// can import FromClient, FromServer, and related types alongside the legacy
// ActorEvent-based types.
import { cp, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));
const target = path.join(root, '..', '..', 'vscode', 'dist');

await mkdir(path.join(target, 'sidebar'), { recursive: true });
await cp(path.join(root, 'dist', 'protocol.js'), path.join(target, 'protocol.js'));
await cp(path.join(root, 'dist', 'protocol.d.ts'), path.join(target, 'protocol.d.ts'));
await cp(path.join(root, 'dist', 'sidebar', 'messages.js'), path.join(target, 'sidebar', 'messages.js'));
// Transform the sidebar messages.d.ts to use Record<string, unknown>[] for
// events instead of ActorEvent[], since the vscode host now emits ServerNote
// notifications (not legacy ActorEvent objects).
let messagesDts = await readFile(path.join(root, 'dist', 'sidebar', 'messages.d.ts'), 'utf8');
messagesDts = messagesDts.replace(/events: ActorEvent\[\];/g, 'events: Record<string, unknown>[];');
await writeFile(path.join(target, 'sidebar', 'messages.d.ts'), messagesDts);

// Merge the AgentServer protocol types (ts-rs generated bindings) into the
// staged protocol.d.ts so the vscode host can import both the legacy types
// (ActorEvent, Command, etc.) and the AgentServer protocol types (FromClient,
// FromServer, etc.) from the same module.
const agentServerBindings = path.join(root, '..', '..', 'crates', 'manox-protocol', 'bindings', 'protocol.ts');
const agentServerContent = await readFile(agentServerBindings, 'utf8');
const existingDts = path.join(target, 'protocol.d.ts');
const existingContent = await readFile(existingDts, 'utf8');

// Remove the import line from the agent-server bindings (it imports JsonValue
// which we handle separately by adding a re-export) and append the AgentServer types.
const lines = agentServerContent.split('\n');
const cleanLines = lines.filter((l) => !l.startsWith("import type { JsonValue }"));
let merged = existingContent.trimEnd() + '\n\n// ── AgentServer protocol types (ts-rs generated) ────────────────\n' + cleanLines.join('\n');

// Add JsonValue re-export if not already present.
if (!merged.includes('export type { JsonValue }') && !merged.includes("export type { JsonValue } from")) {
  // JsonValue is needed by the AgentServer types; add a re-export at the
  // very top of the file, before any type definitions.
  const importLine = "export type { JsonValue } from './serde_json/JsonValue';\n";
  merged = importLine + merged;
}

await writeFile(existingDts, merged);

// Also stage the serde_json/JsonValue type used by the AgentServer protocol.
const serdeJsonTarget = path.join(target, 'serde_json');
await mkdir(serdeJsonTarget, { recursive: true });
await cp(path.join(root, '..', '..', 'crates', 'manox-protocol', 'bindings', 'serde_json', 'JsonValue.ts'), path.join(serdeJsonTarget, 'JsonValue.d.ts'));

console.log('contracts staged into vscode/dist');
