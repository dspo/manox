# manox-codemode

QuickJS script sandbox with a tool bridge. The model writes a JS script (an
async function body) that runs in a QuickJS VM; the script's only capability
that leaves the VM is `tools.<name>(args)` — calls into the tools injected at
executor construction. Only the script's `text()`/`console` output and its
`return` value reach the caller; the raw results of nested tool calls never
enter the model's context. The crate has **zero manox-harness dependencies**:
callers adapt their own tool type to [`CodemodeTool`].

This README records the MR1 spike conclusions (PR1 of the codemode plan).

## Spike conclusions

### 1. Bridge form: promise bridge with a dedicated VM thread (not `AsyncRuntime`)

The rejected alternative was rquickjs's `AsyncRuntime`/`AsyncContext`
(the `futures` feature). Conclusions from the spike:

- `AsyncRuntime` couples the VM's driving to the host executor: the VM's job
  queue is pumped by an Rust future, which means every script resumption is a
  task spawn on the host runtime and the "script suspended, queue empty"
  window — where hang detection and result delivery live — is implicit inside
  the executor instead of an explicit loop we own.
- It would pull the `futures` feature (async-lock dependency) and put the
  runtime on a thread whose whole purpose is to feed one script; the promise
  bridge gives the same concurrency with zero extra features.
- The chosen design: prelude-held resolvers (a JS `Map` of pending call ids to
  `{resolve, reject}`), natives that only **send** (`hostCall` hands the call
  to the host channel and returns immediately — never blocks), and a host-side
  relay that answers with `{call_id, ok, payload}` messages. Results return
  out of order and pair back by id, so `Promise.all` in the script stays
  genuinely concurrent (proved by a barrier test: eight calls all in flight at
  once, plus a reversed-completion-order test).

rquickjs borrow discipline shaped the final loop shape (the one real surprise
of the spike): **`Context::with` holds the runtime's `RefCell` lock for the
whole closure**, so `Runtime::execute_pending_job` cannot be called inside a
`with` — that panics with "RefCell already borrowed". The loop therefore
alternates: setup in one `with`; then outside it, drain the job queue
(`execute_pending_job`), poll `pendingCount` and settle results through short
`with` re-entries, with the prelude API object kept alive across those windows
by `rquickjs::Persistent`. The prelude API (`run`/`settle`/`pendingCount`) is
reachable only from Rust; the bridge natives are closure-captured parameters
of the prelude factory and never globals, so the script cannot touch either.

### 2. Hang detection: pending-count probe when the job queue drains

Equivalent of Pi's `stalled()` check, implemented without an executor idle
callback: after the job queue drains, if the script has not reported `done`,
and the prelude reports **zero pending tool calls**, the script is waiting on
a promise nothing can ever settle (there are no timers, no IO, no module
events in the VM) — the execution fails immediately with a script error whose
message says exactly that. Verified by test: `await new Promise(() => {})`
fails in milliseconds, not after a timeout. A script suspended on a real tool
call reports `pendingCount > 0` and the loop blocks on the result channel
instead.

### 3. Cancellation paths (measured)

- **Pure JS loop** (`while (true) {}`): the `set_interrupt_handler` closure
  polls an `Arc<AtomicBool>` and QuickJS raises an uncatchable exception at an
  opcode-count poll point; the script's own `catch` cannot intercept it. The
  job-drain loop additionally checks the flag between jobs, so
  `while (true) await null` (one job per iteration) cannot run long between
  polls. Ends as `aborted`.
- **Suspended on a tool result**: the result-wait loop uses
  `recv_timeout(25 ms)` and steps the flag each tick, so cancellation reaches
  a VM that is blocked waiting rather than executing. Ends as `aborted`; the
  in-flight call's derived `CancellationToken` fires and its call record stays
  `cancelled`.
- **Timeout** (`@options.timeout_ms`): the host owns the absolute deadline
  (`tokio::time::sleep_until` in the relay `select!`); firing takes the same
  terminal path and cancels the in-flight calls too. Ends as `timeout`.
- First terminal wins: a `done` that races the deadline still reports the
  script's own outcome; after any terminal, the VM thread exits promptly
  (interrupt polls + recv steps) and is joined with a 3 s budget — a thread
  that misses it detaches and cleans up its own runtime, which the host never
  touches.

### 4. Engine pin: rquickjs `=0.14.0`, features = `["std"]` only

- `=0.14.0` exact pin: 0.14 is the post-overhaul line (API and feature set
  still break between minors); upgrading is an explicit task.
- Backend is quickjs-ng (vendored by rquickjs-sys; **0.16.2** in 0.14.0),
  built with `cc` from the vendored C sources — no external toolchain, no
  zig, no network fetch at build time.
- `default-features = false, features = ["std"]`: `loader`/`dyn-load` are
  exactly the module-loading capability the sandbox must not have (dynamic
  `import()` fails as expected — tested); `futures` pulls async-lock and the
  `AsyncRuntime` shape rejected above; `rust-alloc`/`allocator` would swap in
  a custom allocator and **silently disable `set_memory_limit`** (the limit is
  a property of QuickJS's own allocator), so the engine's allocator stays.
- Guardrails verified by test: memory limit 256 MiB raises a **catchable
  `InternalError: out of memory`**; stack guard 1 MiB turns deep recursion
  into a **catchable `RangeError: Maximum call stack size exceeded`**.

### 5. Footprint (macOS arm64, quickjs-ng 0.16.2)

- `libquickjs.a` static archive, release profile: **1,452,784 bytes
  (~1.39 MiB)**; debug profile: 2,582,904 bytes (~2.46 MiB). The archive is
  linked into whatever binary consumes the crate; there is no dynamic library
  and no runtime asset.
- `libmanox_codemode.rlib` (release, this crate's own code + embedded
  prelude): ~1.2 MiB including metadata; the linked-in increment for a
  consumer is dominated by the quickjs archive above. Consistent with the
  plan's ~1–2 MiB estimate and an order of magnitude under the RustPython
  stack it is slated to replace (PR3 decides via D16/D17).

## Deviations from the plan's letter (all vetted against its intent)

- **Allow-list check layer**: the plan sketched `__hostCall` validating the
  tool name and throwing a JS Error. Implemented with the same observable
  contract one layer up: the `tools` proxy in the prelude rejects unknown
  members when called (a rejected promise carrying a `TypeError` that lists
  available tools), so unknown calls never leave the VM and produce no call
  record. The host re-validates every incoming call against its tool map
  anyway (fail-closed against tampered bridge traffic); malformed args JSON
  and over-cap calls reject through the same path.
- **Bridge natives never block, and the "recv window" lives outside any
  `with`**: see spike conclusion 1. This is the rquickjs-native spelling of
  the plan's "VM thread enters the recv loop in the queue-empty window".
- **No built-in lockdown in the prelude** (Pi freezes its intrinsics): the
  prelude shares the VM with the script, so a script that patched
  `JSON.stringify` could send malformed bridge traffic. Rather than port the
  lockdown machinery, every payload the host receives is parsed defensively
  and any malformed traffic fails the execution as a **sandbox** error
  (store writes, error descriptions, call args). The VM is process-internal
  and one-shot per execution; the threat model (model-written scripts, not
  memory-safe-attacker-proof) is unchanged.
- **Result surface naming**: the soft output limit keeps head + tail with an
  omission marker in `CodemodeResult.text` and carries the full text in
  `CodemodeResult.full_output` (the plan: "全文单独放在结果结构里").
  `output` stays the raw item record, including partial output on failure.
  The soft budget counts text characters only; image bytes count toward the
  hard 16 Mi-char / 100k-item limits, which the prelude enforces mid-run.
- **`describeTool` for budget-dropped tools** returns the description only
  (the declaration did not fit the budget; the host learns the dropped list
  from `rendered_declarations()`).
- **`exit()` semantics**: `exit()` reports success immediately and unwinds the
  script by throwing a sentinel; a subsequent rejection handler is a no-op
  (the prelude's `done` guard is first-terminal-wins inside the VM too).

## Library surface

- [`CodemodeExecutor::new(Vec<Arc<dyn CodemodeTool>>)`] — rejects duplicate
  names and names that normalize to the same script identifier; renders the
  tools' TypeScript declarations once. Two budgets bound the listing: 3000
  est-tokens per tool (tag drop, then whole-tool drop) and a shared 4000
  est-tokens across all samples (first come, first served; a tool that does
  not fit the remaining pool drops whole to the name-only hint). On the main
  assembly's core surface the shared pool admits Read/Write/Edit/Bash/
  BashOutput/Grep/Glob/Ls (~18.4k characters, ~4.6k est-tokens including the
  prompt), versus ~8.8k est-tokens with the per-tool budget alone.
- [`CodemodeExecutor::execute(source, store, cancel)`] — requires a tokio
  runtime context (nested calls are `tokio::spawn`ed). Never returns `Err`:
  failures are `CodemodeResult.error` (kinds `script` / `timeout` /
  `aborted` / `sandbox`), and every result carries the output produced before
  the failure. `store` in, `store_writes` out (empty unless the script
  succeeded); the crate never touches the disk. `wall_time` covers the whole
  call including any wait for a process-wide slot.
- Process-wide admission: at most `MAX_CONCURRENT_EXECUTIONS` (2) executions
  run at once across the process, queued in arrival order; a wait past
  `EXECUTION_QUEUE_TIMEOUT` (60 s) fails as a `sandbox`-kind result (a host
  resource, not a script fault), and a caller cancel during the wait ends as
  `aborted`. The slot is held for the whole run.
- Global in-flight cap of 32 nested calls (`MAX_IN_FLIGHT_CALLS`); overflow
  rejects immediately with an explanatory error. Call records cap at
  `MAX_RECORDED_CALLS` (100) in the result, with `total_calls` carrying the
  true count. Per-tool sequential-execution semantics are the MR2 adapter's
  job, not the bridge's.

The script surface itself is the caller's decision: the harness-side
`CodemodeTool` is built over the session's assembly-time active set (the
mounted tools minus opt-in suites such as the browser suite), so a dormant
suite is neither callable nor advertised. Two fail-closed staleness classes
follow: a suite enabled after assembly and an embedder tool registered later
are not script-callable until the next assembly.

## Testing

`cargo test -p manox-codemode` — 25 unit tests (source parsing, store
writes decoding, declaration rendering snapshots incl. both budgets, output
rendering, error JSON shape) and 27 end-to-end tests against a live VM:
isolation, true concurrency (`Promise.all` barrier), out-of-order result
pairing, cancellation (loop, suspension, and during the slot queue), timeout,
hang detection, memory and stack guards, soft/hard output limits, console
block ordering, image validation, `@options` branches, store round-trip and
limits, declaration discovery (`ALL_TOOLS`/`describeTool`), tool error text
propagation, the in-flight and record caps, the process-wide slot cap,
`return`/`exit` semantics, and syntax errors with aligned line numbers.
