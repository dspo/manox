// Dev server for the browser host: serves the webview bundle with a token
// injected into the page, and tunnels /ws upgrades straight through to a
// running Manox.app. Iterating on the UI never touches cargo — the browser
// talks to the same actor behind the same messages.ts contract.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as esbuild from 'esbuild';

const root = path.dirname(fileURLToPath(import.meta.url));
const port = parseInt(process.env.PORT ?? '4173', 10);
const manoxPort = parseInt(process.env.MANOX_PORT ?? '8090', 10);
const token = process.env.MANOX_TOKEN ?? '';

// Watch-rebuild the bundle into dist/webview so the static handler below
// always serves the latest build.
const ctx = await esbuild.context({
  entryPoints: [path.join(root, 'src', 'sidebar', 'webview', 'main.tsx')],
  bundle: true,
  outfile: path.join(root, 'dist', 'webview', 'bundle.js'),
  format: 'iife',
  target: 'es2022',
  jsx: 'automatic',
  logLevel: 'info',
});
await ctx.watch();

const html = await readFile(path.join(root, 'index.html'), 'utf8');

const server = createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  if (url.pathname === '/') {
    const page = html.replace('__MANOX_TOKEN__', token);
    res.writeHead(200, { 'Content-Type': 'text/html' });
    res.end(page);
    return;
  }
  const file = path.join(root, 'dist', 'webview', url.pathname.slice(1));
  try {
    const data = await readFile(file);
    const type = url.pathname.endsWith('.js')
      ? 'application/javascript'
      : url.pathname.endsWith('.css')
        ? 'text/css'
        : 'application/octet-stream';
    res.writeHead(200, { 'Content-Type': type });
    res.end(data);
  } catch {
    res.writeHead(404);
    res.end();
  }
});

// Blind HTTP->TCP tunnel: replay the client's upgrade request verbatim at the
// Manox.app port (query token included) and pipe the 101 + subsequent frames
// back. The browser never learns the backend address.
server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url, 'http://localhost');
  const backend = net.connect(manoxPort, '127.0.0.1');
  let headerSent = false;
  const sendHeader = () => {
    if (headerSent) return;
    headerSent = true;
    const lines = [
      `${req.method} ${url.pathname}${url.search} HTTP/1.1`,
      `Host: 127.0.0.1:${manoxPort}`,
      ...Object.entries(req.headers)
        .filter(([k]) => k !== 'host')
        .map(([k, v]) => `${k}: ${v}`),
    ];
    backend.write(lines.join('\r\n') + '\r\n\r\n');
  };
  backend.on('connect', () => {
    sendHeader();
    if (head.length > 0) backend.write(head);
  });
  backend.on('error', () => socket.destroy());
  socket.on('error', () => backend.destroy());
  backend.pipe(socket);
  socket.on('data', (chunk) => {
    if (headerSent) backend.write(chunk);
  });
});

server.listen(port, () => {
  console.log(`webui dev server: http://127.0.0.1:${port} (ws -> Manox.app :${manoxPort})`);
});
