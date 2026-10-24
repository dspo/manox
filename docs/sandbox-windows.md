# Windows sandbox selection — restricted token leads, MXC watches

> **Normative selection, not an as-built record.** The two-tier access modes
> (`sandbox` / `full-access`) are macOS-first (seatbelt deny-default base) with
> Linux (bubblewrap + seccomp) queued next. Windows has no deliverable build
> today, so nothing in this document is implemented; this page freezes the
> selection so the future Windows leg starts from a decision instead of a
> survey. Enforcement facts are distilled from openai/codex at `de06fd0be4`
> (its `codex-rs/{sandboxing,windows-sandbox-rs,windows-sandbox-service,mxc-sandbox}`
> as-built) — selection facts only, no code reused. Until a Windows deliverable
> exists, this document is the leg's only deliverable, and every gate in §4 is
> a contract the future implementation PR must pass.

## 1. Decision

**Main path — four cooperating OS mechanisms, all provisioned without a
container image or a VM:**

1. **Restricted token.** Spawn the workload with
   `CreateRestrictedToken` from the current process token, flags
   `DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED`, launched via
   `CreateProcessAsUserW`. This is the unelevated default: no admin prompt, no
   helper account, works from a plain user session.
2. **Per-workspace capability SIDs over NTFS ACLs.** One persistent random
   `S-1-5-21-…` SID per workspace / writable root, stored next to the manox
   state home and reused across runs. Setup appends deny ACEs for the
   workspace's capability SID on everything outside that workspace's write
   scope (and per-root deny carveouts), so the restricted token — which
   carries the workspace's capability SID — can write the workspace and is
   denied elsewhere. Persistent SIDs keep stale ACLs from expanding the write
   scope of later, narrower policies.
3. **WFP firewall at the ALE layer.** Network confinement is Windows Filtering
   Platform, not in-process interception: persistent provider/sublayer/filters
   with `FWPM_CONDITION_ALE_USER_ID` conditions keyed on the sandbox
   identity's SID, so only sandboxed processes' connections are filtered.
   Filters are installed once per account and persist across reboots.
4. **Private desktop.** GUI-bearing children launch on a `CreateDesktopW`
   desktop with a restrictive DACL (no hook control, no journal playback, no
   read-objects for other desktops), named in `STARTUPINFO` as
   `Winsta0\<name>`. This denies cross-desktop window-message injection and
   UI hooks from a sandboxed process into the user's interactive desktop.

**Watchlist — Microsoft MXC (PSEC).** The `ProcessSecurityEnvironment` API is
the strategic direction (kernel-mediated policy, no ACL mutation, no accounts),
but it exists only on new Windows builds. If MXC is ever selected, availability
is decided by a **real create-and-close probe** against the PSEC API with a
deny-all egress policy — never by OS build number, never by the SDK's broad
platform check, which also reports older AppContainer backends as supported.
`permissiveLearningMode` (the enforcement-learning escape hatch) is explicitly
rejected: a sandbox that is still learning is not a sandbox. MXC stays
opt-in-strict and never a fallback: a failed MXC launch fails the command, it
does not rerun unsandboxed.

**Product semantics — opt-in, like codex.** Codex ships `windows.sandbox`
defaulting to `Disabled`, with `RestrictedToken` and `Elevated` as the
explicit levels, and MXC behind a default-off flag. Manox matches: on Windows
the `sandbox` access mode is **off until the user turns it on**, and an
explicitly requested sandbox that cannot be provisioned fails closed (§2) —
"unavailable" is a visible error, never a silent downgrade to unsandboxed
execution.

## 2. Limits and pitfalls (as-built facts from codex)

- **Deny-read is not expressible unelevated — the headline constraint.** A
  `WRITE_RESTRICTED` token's capability SIDs participate in **write** access
  checks only; during **read** checks the capability SID's deny ACEs are not
  adopted. Consequence: any policy needing read denial (deny-read paths,
  split read/write roots) cannot be enforced by the unelevated backend. The
  resolution rule is fail-closed: **refuse to run the call** with a
  diagnostic saying the sandbox cannot enforce the policy — never run it
  unsandboxed. This is codex's actual behavior (`windows unelevated
  restricted-token sandbox cannot enforce deny-read restrictions directly;
  refusing to run unsandboxed`) and it is manox's rule too.
- **WFP write access needs an administrator.** Installing or removing
  persistent filters requires opening the filter engine for write, which
  requires elevation. Codex therefore ships a SYSTEM service
  (`windows-sandbox-service`) that owns filter lifecycle and a setup path that
  runs elevated. For manox this is real install/upgrade surface cost: the
  Windows deliverable carries a service (or an equivalent elevated
  provisioning step) whose install, upgrade, and uninstall must be designed,
  not improvised at first launch.
- **Elevated backend = dedicated sandbox account + `CreateProcessWithLogonW`.**
  The elevated level creates a local sandbox account (random password,
  DPAPI-protected at rest), installs the WFP filters against *that* account's
  SID (which is what makes ALE-layer targeting possible), and launches
  workloads with `CreateProcessWithLogonW`. This is the only tier that can
  enforce deny-read (the account's ACL view is a normal user's). It exists as
  a level, not a requirement: the restricted-token main path must stand alone.
- **Policy shapes that do not project must fail closed, same as deny-read.**
  Codex refuses (rather than approximates) when: writable root sets differ
  from the policy's legacy projection, a writable descendant would be reopened
  under a read-only carveout, or the filesystem root is not readable while
  read restrictions exist. Manox's rule: a resolution the Windows main path
  cannot express is an error naming the unenforceable piece.
- **ConPTY is its own spawn path.** PTY workloads need
  `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` plumbing (`CreateProcessAsUserW` +
  `STARTUPINFOEXW`), shared by the restricted-token and elevated paths. The
  bash registry's PTY variant must exercise the same confinement as the pipe
  variant; the ConPTY path is a frequent source of "works with pipes, leaks
  with a TTY" bugs, so it gets its own smoke gate (§4).
- **Private desktop lifecycle.** Desktops are per-launch or cached per sandbox
  identity, closed deterministically (a handle still in use fails the close —
  treat as a bug, not a warning), and never fall back to the interactive
  `Default` desktop. A sandboxed GUI child on the user's desktop silently
  undoes the isolation.
- **MXC specifics worth remembering when it matures:** deny paths additionally
  require the `PSE_SUPPORT_FS_DENY` capability; volume-root grants do not
  recurse into directories mounted later; descendants are killed when the
  foreground process exits (codex's other backends preserve them — detached
  servers behave differently); environments must be Unicode without lossy
  conversion.

## 3. Alignment with manox constraints

- **Single binary, self-invocation — but no argv wrapper on Windows.** The
  macOS seatbelt path wraps the command in a profile-bearing launcher and the
  planned Linux path self-execs the same binary as an inner helper via hidden
  arguments. Codex mirrors that shape on Windows with an
  `--run-as-windows-sandbox` argv wrapper for parity. Manox does **not**:
  the restricted spawn is a spawn-time operation, so the bash registry's
  Windows backend **replaces the spawn function** (build token →
  `CreateProcessAsUserW` with the prepared environment/attrs) instead of
  re-invoking the binary through a sentinel argument. No hidden-arg protocol,
  no wrapper detectable in the child's argv, and one less argv-length budget
  to worry about.
- **The access-mode vocabulary does not fork per platform.** `sandbox` /
  `full-access` (the persisted `AccessMode`), the writable-root derivation
  (workspace root, `~/.manox`, platform temp), the `require_escalated` +
  `justification` single-call grant with strict widening, and the
  `[sandbox: …]` denial marker + same-turn escalation hint are all
  platform-independent contracts in `manox-harness::ext::sandbox`. Windows
  plugs in under them: what changes is only the enforcement family (token +
  ACLs + WFP + desktop instead of seatbelt) and the post-exit denial
  signature list (the Windows access-denied family maps to the same marker
  path that seatbelt's "operation not permitted" uses today).
- **Fail-closed is already the house style.** The escalation path refuses
  without an approver, refuses narrowing, and refuses unknown persisted
  values by landing on the bounded default. The Windows leg inherits the same
  posture: an unenforceable requested policy is a refusal with a reason, and
  the refusal text suggests the escalation route — a request manox can deny
  is never silently widened.
- **Scope guard.** No Windows implementation exists in this repo and none is
  started by this document. When a Windows deliverable build lands, the work
  splits into at least: (a) service/provisioning surface, (b) restricted-token
  spawn backend, (c) policy resolution + fail-closed rules, (d) smoke suite
  (§4). This page is the selection each of those must conform to.

## 4. Acceptance smoke checklist (for the future implementation PR)

Every item runs on a real Windows host, in both a plain (non-admin) session
and — where the elevated tier is involved — an elevated one:

1. **Write fence.** Sandboxed write inside the workspace root succeeds; write
   outside every writable root (e.g. the user profile outside `~/.manox`,
   `C:\Program Files`, another workspace's root) fails with the sandbox
   denial marker surfaced, not a bare crash.
2. **Deny-read fail-closed.** A policy carrying a deny-read path (or any
   unprojectable read restriction) under the unelevated backend **refuses to
   run**: the call returns the fail-closed diagnostic; nothing executes
   unsandboxed.
3. **WFP interception.** With network confinement on, a sandboxed process's
   direct egress is blocked (or restricted to the profile's loopback grants);
   an unsandboxed sibling process is unaffected; filters survive a service
   restart and are removed by uninstall.
4. **Desktop isolation.** A sandboxed GUI process cannot read the interactive
   desktop's windows or inject input into it; its own private desktop is
   closed on exit with no orphans; a killed child does not leak the desktop
   handle.
5. **Escalation contract.** `require_escalated: true` + justification grants
   full access for exactly one call; the immediately following call is
   re-confined; a rejection leaves the session's standing mode untouched.
6. **ConPTY smoke.** `powershell.exe` and `pwsh.exe` under a real PTY through
   the restricted-token path (and the elevated path, once present), with the
   same write/network fences asserted as in the pipe variant.
