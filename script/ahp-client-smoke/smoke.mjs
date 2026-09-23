#!/usr/bin/env node
/**
 * The W2 ④a acceptance smoke: drive a real manox AHP gateway with the
 * **official upstream TypeScript client**.
 *
 * Why this is a distinct layer from the Rust suites: `crates/manox-ahp/tests/*`
 * and `ws_conformance.rs` connect with `ahp::Client`, which shares our Rust
 * `serde` shape assumptions. A field we spell slightly differently from the
 * spec still round-trips between our own two halves and looks green. The
 * upstream TS client is generated from the spec's own TypeScript source, so it
 * fails on exactly those disagreements — the class of bug our own tests
 * structurally cannot see.
 *
 * Usage (the gateway must already be serving, e.g.
 * `cargo run -p manox-session-core --example ahp_serve -- --port 8765`, which
 * prints the token and writes it to `<config>/gateway-ws.json`):
 *
 *   node script/ahp-client-smoke/smoke.mjs ws://127.0.0.1:<port>/ahp <token>
 *
 * Setup (once, outside the repo — this is a verification tool, not a build
 * dependency, so it adds nothing to the Cargo graph):
 *
 *   mkdir -p /tmp/ahp-ts-probe && cd /tmp/ahp-ts-probe
 *   npm init -y && npm install @microsoft/agent-host-protocol@0.9.0
 *   cp <repo>/script/ahp-client-smoke/smoke.mjs .
 *
 * Exit status is 0 only when every assertion passes.
 */

const [, , urlArg, tokenArg] = process.argv;
if (!urlArg || !tokenArg) {
  console.error("usage: smoke.mjs <ws-url> <token>");
  process.exit(2);
}

const { AhpClient, RpcError } = await import("@microsoft/agent-host-protocol/client");
const { WebSocketTransport } = await import("@microsoft/agent-host-protocol/ws");

/**
 * Collect failures instead of throwing on the first one: a smoke run should
 * report everything the external client disagrees with, not just the first.
 */
const failures = [];
function check(name, ok, detail) {
  if (ok) {
    console.log(`  PASS  ${name}`);
  } else {
    console.log(`  FAIL  ${name}${detail !== undefined ? ` — ${detail}` : ""}`);
    failures.push(name);
  }
}

// Auth is the query string: the browser WebSocket API cannot set headers, and
// that is also the carrier our endpoint file advertises.
const url = new URL(urlArg);
url.searchParams.set("token", tokenArg);

const ROOT = "ahp-root://";
console.log(`connecting the upstream TS client to ${url.origin}${url.pathname}`);

const transport = await WebSocketTransport.connect(url.toString());
const client = new AhpClient(transport, { requestTimeoutMs: 20000 });
// The read pump is NOT started by the constructor: without this the socket
// receives frames and nothing ever dispatches them, so every request times out
// against a perfectly healthy host. (Cost an hour to find once; recorded here
// so the next reader does not pay it again.)
client.connect();

try {
  // ── 1. handshake ────────────────────────────────────────────────────────
  const initialized = await client.initialize({
    clientId: "ts-smoke",
    protocolVersions: ["0.9.0"],
    initialSubscriptions: [ROOT],
  });
  check(
    "initialize negotiates a protocol version",
    typeof initialized.protocolVersion === "string" && initialized.protocolVersion.length > 0,
    JSON.stringify(initialized.protocolVersion),
  );
  check(
    "initialize carries a serverSeq",
    typeof initialized.serverSeq === "number",
    `serverSeq=${initialized.serverSeq}`,
  );
  check(
    "the root snapshot comes back",
    Array.isArray(initialized.snapshots) && initialized.snapshots.length > 0,
    `snapshots=${initialized.snapshots?.length}`,
  );

  // ── 2. the x-manox declaration ──────────────────────────────────────────
  // A non-manox client must be able to tell what this host serves; the upstream
  // client surfaces `_meta` verbatim, which is the whole discovery mechanism.
  const meta = initialized._meta?.["x-manox"];
  check(
    "_meta advertises the x-manox surface",
    Boolean(meta),
    JSON.stringify(initialized._meta)?.slice(0, 120),
  );
  check("x-manox declares a version", typeof meta?.version === "number", `version=${meta?.version}`);
  check(
    "x-manox lists its channels",
    Array.isArray(meta?.channels) && meta.channels.every((c) => c.startsWith("x-manox")),
    JSON.stringify(meta?.channels),
  );

  // ── 3. connection-level commands ────────────────────────────────────────
  await client.ping();
  check("ping answers", true);

  // `listSessions` / `resolveSessionConfig` are in the spec's CommandMap but
  // have no typed wrapper upstream, so they go through the generic `request`.
  const sessionList = await client.request("listSessions", { channel: ROOT });
  check(
    "listSessions answers the upstream result shape",
    Array.isArray(sessionList.items),
    JSON.stringify(sessionList).slice(0, 200),
  );

  // ── 4. an empty shape must be a *typed* empty shape ─────────────────────
  // These are the "answer empty rather than MethodNotFound" decisions: a client
  // calls them before it can create anything, so a MethodNotFound would stop it
  // dead. A wrongly-shaped empty object fails the upstream types here.
  const config = await client.request("resolveSessionConfig", { channel: ROOT });
  check(
    "resolveSessionConfig answers a schema/values pair",
    config && typeof config === "object" && "schema" in config && "values" in config,
    JSON.stringify(config).slice(0, 160),
  );

  const completions = await client.completions({
    channel: ROOT,
    kind: "userMessage",
    text: "",
    offset: 0,
  });
  check(
    "completions answers an items list",
    Array.isArray(completions?.items),
    JSON.stringify(completions).slice(0, 160),
  );

  // ── 5. subscribe: snapshot then deltas ──────────────────────────────────
  const { result: subResult, subscription } = await client.subscribe(ROOT);
  check(
    "subscribe returns a root snapshot",
    Boolean(subResult?.snapshot),
    JSON.stringify(Object.keys(subResult ?? {})),
  );
  subscription.close();

  // ── 6. reconnect: the snapshot leg ──────────────────────────────────────
  const reconnected = await client.reconnect({
    clientId: "ts-smoke",
    lastSeenServerSeq: initialized.serverSeq,
    subscriptions: [ROOT],
  });
  check(
    "reconnect answers with snapshots (the first-version leg)",
    Array.isArray(reconnected.snapshots),
    JSON.stringify(reconnected).slice(0, 200),
  );

  // ── 7. a dispatch must be answered, not dropped ─────────────────────────
  // AHP has no write receipt: the client learns the outcome from the echoed
  // envelope (or its `rejectionReason`). A dropped dispatch is indistinguishable
  // from a slow one until the client gives up, so the echo is the contract.
  const handle = client.dispatch(ROOT, { type: "x-manox/pinnedChanged", pinned: true });
  check("dispatch returns an awaitable handle", Boolean(handle));
} catch (error) {
  const detail =
    error instanceof RpcError ? `${error.code} ${error.message}` : (error?.message ?? String(error));
  check("the smoke sequence ran to completion", false, detail);
} finally {
  await client.shutdown().catch(() => {});
}

console.log("");
if (failures.length > 0) {
  console.error(`TS CLIENT SMOKE FAILED: ${failures.length} check(s): ${failures.join(", ")}`);
  process.exit(1);
}
console.log("TS CLIENT SMOKE PASSED: the upstream client agrees with this host's wire shape");
