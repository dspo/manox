// Copy the compiled contract modules (protocol/messages) under vscode/dist
// so the host's tsc emits require()s that resolve both in development and
// inside the packaged vsix (vsce packages vscode/dist unconditionally). The
// sources stay owned by webui/.
import { cp, mkdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));
const target = path.join(root, '..', 'vscode', 'dist');

await mkdir(path.join(target, 'sidebar'), { recursive: true });
await cp(path.join(root, 'dist', 'protocol.js'), path.join(target, 'protocol.js'));
await cp(path.join(root, 'dist', 'protocol.d.ts'), path.join(target, 'protocol.d.ts'));
await cp(path.join(root, 'dist', 'sidebar', 'messages.js'), path.join(target, 'sidebar', 'messages.js'));
await cp(path.join(root, 'dist', 'sidebar', 'messages.d.ts'), path.join(target, 'sidebar', 'messages.d.ts'));
console.log('contracts staged into vscode/dist');
