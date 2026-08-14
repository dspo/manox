// P0 smoke test: drive the agent actor from Node and verify the streaming
// event bridge (gpui HeadlessAppContext on a dedicated thread).
//
// Run after `cargo build -p manox-napi` and copying the cdylib to
// `<repo>/target/debug/manox_napi.node` (or use the @napi-rs/cli build):
//   cargo build -p manox-napi
//   cp target/debug/libmanox_napi.dylib target/debug/manox_napi.node
//   node test/p0-smoke.mjs
//
// Requires a live provider (cx.providers.config.yaml) — this is a live-API
// test, the same class as MANOX_RUN_LIVE gated Rust tests.

import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const m = require('../../../target/debug/manox_napi.node');

const assert = (cond, msg) => {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
  console.log(`ok: ${msg}`);
};

const cwd = process.cwd();
const events = [];
let sawText = false;
let sawStop = false;

m.start((err, batch) => {
  assert(err === null, 'callback error-first argument is null');
  for (const raw of batch) {
    const ev = JSON.parse(raw);
    events.push(ev.type);
    if (ev.type === 'agent_text') sawText = true;
    if (ev.type === 'stop') sawStop = true;
  }
});

assert(m.ping() === 'pong', 'ping returns pong');
assert(typeof m.sendCommand === 'function', 'sendCommand exposed');

m.sendCommand(JSON.stringify({ cmd: 'init', cwd }));
await sleep(1500);
m.sendCommand(JSON.stringify({ cmd: 'create_session', cwd }));
await sleep(1000);
m.sendCommand(JSON.stringify({ cmd: 'submit', text: 'Reply with exactly: OK' }));
await sleep(15000);
m.sendCommand(JSON.stringify({ cmd: 'get_usage' }));
await sleep(1000);

const counts = {};
for (const t of events) counts[t] = (counts[t] || 0) + 1;
console.log('event types:', JSON.stringify(counts, null, 2));

assert(counts['ready'] === 1, 'init emits ready');
assert(counts['session_created'] === 1, 'create_session emits session_created');
assert(counts['turn_started'] === 1, 'submit emits turn_started');
assert(sawText, 'streamed agent_text received');
assert(sawStop, 'turn ended with stop');
assert(counts['usage'] === 1, 'get_usage responds with usage');

console.log('\nP0 smoke: PASS');
process.exit(0);

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}
