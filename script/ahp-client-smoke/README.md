# AHP client smoke (W2 gate ④a)

Drives a **real manox AHP gateway** with the **official upstream TypeScript
client**, as an independent check on the wire shape.

## Why this exists

The Rust suites (`crates/manox-ahp/tests/ws_conformance.rs`, the in-process
e2e, the session-core suites) all connect with `ahp::Client` — our own Rust
client, which shares the host's `serde` shape assumptions. If we spell a field
differently from the spec, that mistake round-trips between our two halves and
every test still passes.

The upstream TS client is generated from the specification's own TypeScript
source, so it disagrees exactly where we do. That is the class of bug this
layer catches and the Rust suites structurally cannot.

## Running it

The package is intentionally **not** a repo dependency (it would add a Node
toolchain to the build graph). Install it beside the repo:

```bash
mkdir -p /tmp/ahp-ts-probe && cd /tmp/ahp-ts-probe
npm init -y
npm install @microsoft/agent-host-protocol@0.9.0   # must match the pinned ahp-types
cp <repo>/script/ahp-client-smoke/smoke.mjs .
```

Start a gateway and run the smoke:

```bash
# Terminal 1 — the gateway prints the token and writes ~/.manox/gateway-ws.json
cargo run -p manox-session-core --example ahp_serve -- --port 0

# Terminal 2
node smoke.mjs ws://127.0.0.1:<port>/ahp <token>
```

Exit status is 0 only when all 13 assertions pass.

## Two traps, recorded so they are paid for once

1. **`AhpClient` does not start its read pump in the constructor.** You must
   call `client.connect()`. Without it the socket receives frames and nothing
   dispatches them, so *every* request times out against a perfectly healthy
   host — it looks exactly like a server bug.
2. **`completions` takes `kind` + `text` + `offset`** (a number), not
   `position`/`line`/`character`. A host refusing a `kind`-less request with
   `-32602` is correct.

Also: make sure the `ahp_serve` process you dial is a **current build**. A
stale binary predating a method answers `-32601` for it, which reads like a
regression until you check `ps` against `target/`.
