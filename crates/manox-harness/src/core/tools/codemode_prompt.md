Run a JavaScript script in a sandboxed QuickJS VM that orchestrates the other
tools of this session. The script is an async function body: `await`,
`Promise.all`, loops, and a top-level `return` all work. Its only capability
that leaves the sandbox is `tools.<name>(args)` — calls into this session's
own tools, wrapped exactly as the top-level calls are (approvals, locks, and
fences apply to every nested call). The nested results never enter the
conversation: only what the script `text()`-prints, `console`-logs, or
`return`s comes back, so this is the tool for fanning out over many calls and
filtering the results down before they reach the context.

Prefer the ordinary declarative parallelism first: issuing several tool calls
in one batch already runs them concurrently. Use Codemode only when the next
call depends on a previous result — dynamic fan-out, filtering or aggregating
large outputs (grep/read over many files, many bash probes), or branching on
results — so the bulk never reaches the context at all.

Environment contract:

- `tools.<name>(args)` — one awaited call; resolves to the tool's result
  object `{ output, isError, images?, details? }` (`output` is the tool's
  text; a failure rejects the promise with the tool's error text —
  `Promise.allSettled` keeps partial success usable). Names normalize for
  property access (`mcp__a-b` → `tools.mcp__a_b`); `tools["<original>"]`
  always works. `ALL_TOOLS` lists every callable name and `describeTool(name)`
  prints one tool's schema — including tools whose declaration did not fit
  this description.
- `text(v)` / `console.log|info|warn|error|debug(...)` — add output items;
  non-strings are JSON-encoded. Console lines render after the body in a
  `<console_output>` block.
- `image(v)` — attach an image (base64 data URL, or `{ data, mimeType }`).
- `store(key, value)` / `load(key)` — a small persistent JSON map carried by
  the session across Codemode calls; `store(key, undefined)` deletes.
  Compressed sessions drop it.
- First line `// @options: {"max_output_tokens": N, "timeout_ms": N}` —
  optional output budget (default 10000 tokens) and a hard wall-clock
  deadline for the whole script including nested calls.
- No timers, fetch, filesystem, or module loading: compute only, everything
  else goes through `tools`.

Interactivity: tools that talk to the user (AskUserQuestion) work inside a
script, but call them once — never fan them out across `Promise.all`, or the
user faces a burst of questions from one script.

Output discipline: `return` the summary you want back (a top-level object is
JSON-rendered), print intermediate findings with `text()` sparingly. When the
output exceeds the budget it is head+tail-truncated and the full text is
saved to a file whose path is reported in the result.
