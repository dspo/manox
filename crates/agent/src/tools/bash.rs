//! `bash` tool — a persistent in-process bash shell backed by [brush].
//!
//! The shell (`brush_core::Shell`) lives for the lifetime of the `BashTool`
//! (one per `Thread`), so `cd` / `export` / function definitions persist across
//! calls — the agent no longer has to re-pin the cwd on every command. Standard
//! builtins (`cd`, `pwd`, `echo`, `export`, `printf`, …) run in-process; only
//! external commands fork.
//!
//! Output is captured via real pipes (`std::io::pipe`) rather than brush's
//! in-memory `OpenFile::Stream`, because brush maps a `Stream` fd to
//! `Stdio::null()` for external children (`brush-core/src/openfiles.rs`:
//! `OpenFile::Stream(_) => Self::null()`) — piping is the only way an external
//! command's stdout/stderr reach us. A pair of `spawn_blocking` readers drain
//! the pipes line-by-line into a capped buffer AND forward each line to the
//! [`ToolOutputSink`] for live UI rendering.
//!
//! Cancellation/timeout semantics mirror the prior `sh -c` harness: each
//! external command runs in its own process group (`NewProcessGroup`), and on
//! timeout/cancel the groups are SIGTERM'd → grace → SIGKILL'd. brush does NOT
//! `kill_on_drop` its children (`brush-core/src/sys/tokio_process.rs`), so we
//! do the reaping ourselves via `Shell::jobs_mut()` after the `run_string`
//! future is dropped.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brush_builtins::ShellBuilderExt;
use brush_core::openfiles::{self, OpenFile, OpenFiles};
use brush_core::results::ExecutionExitCode;
use brush_core::{ExecutionResult, Shell, SourceInfo};
use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, ToolContext, ToolOutputSink};
use crate::tools::{bridge_tokio, schema};

/// Wall-clock limit before a hung command is killed.
const BASH_DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard cap on retained stdout/stderr so a runaway command cannot OOM the app
/// and does not flood the model's context. Above this the capture keeps a
/// running byte total (for the truncation notice) but drops the overflowing
/// text. 64 KiB mirrors the order of magnitude used by other native agents
/// — enough for typical `git`/`cargo` output,
/// small enough that the model can act on it.
const BASH_OUTPUT_MAX_BYTES: usize = 64 * 1024;
/// Narrow-the-command hint folded into the truncation advisory. Lives here
/// (not in `system_prompt.md`) because it is tool-specific guidance.
const BASH_TRUNCATION_HINT: &str =
    "retry with a narrower command (specify columns, `| head`, `LIMIT`, tighten the pattern)";
/// Grace window given to a SIGTERM'd process group before escalating to SIGKILL.
const CANCELLATION_GRACE_MS: u64 = 50;
/// Upper bound on draining the stdout/stderr pipes after the group is killed:
/// grandchildren that inherited the pipes can otherwise keep them open.
const IO_DRAIN_TIMEOUT_MS: u64 = 2_000;

/// Build a brush shell variable marked exported, so it reaches child-process
/// environments. brush assembles a child's env from exported vars only
/// (`ShellEnvironment::iter_exported`, exercised in `brush-core` `commands.rs`);
/// `ShellVariable::new` defaults to non-exported, so `set_env_global` on a
/// plain new var sets a shell-only variable that spawned commands never see.
/// That was the `ed3391e6` failure: `echo $PATH` showed the login PATH and
/// `brew` ran fine (brush's own lookup reads the shell var), but `/usr/bin/which
/// brew` saw the launchd PATH and reported "not found" — the var never reached
/// children. Marking it exported makes `set_env_global` cover external children.
fn exported_shell_var(value: String) -> brush_core::ShellVariable {
    let mut v = brush_core::ShellVariable::new(value);
    v.export();
    v
}

pub struct BashTool {
    /// Base cwd used to seed the shell on first use; per-call overrides persist.
    cwd: PathBuf,
    /// Lazily-initialized persistent brush shell. One per `Thread` (the
    /// `ToolRegistry` is rebuilt per `Thread`).
    shell: Arc<tokio::sync::Mutex<Option<Shell>>>,
    /// Write-confinement policy shared with FS write tools (one derivation per
    /// registry rebuild). The sandboxed path feeds it to `wrap_command`'s
    /// seatbelt; the unsandboxed path uses `is_write_allowed` to reject a `cwd`
    /// override outside the writable set or inside a protected subtree — the
    /// c5aefe4d escape was `cd` into a sibling worktree then git ops against
    /// its `.git`.
    sandbox: crate::sandbox::SandboxPolicy,
    /// Plugin install root for plugin-sourced agents (used to inject
    /// `CLAUDE_PLUGIN_ROOT` into the bash env); `None` for built-in and
    /// user-authored agents. Set by `build_child_registry_with_policy` from
    /// `AgentDefinitionFile.root`.
    plugin_root: Option<PathBuf>,
}

impl BashTool {
    pub fn new(cwd: PathBuf, sandbox: crate::sandbox::SandboxPolicy) -> Self {
        Self {
            cwd,
            shell: Arc::new(tokio::sync::Mutex::new(None)),
            sandbox,
            plugin_root: None,
        }
    }

    /// Construct with a plugin root for `CLAUDE_PLUGIN_ROOT` env injection.
    pub fn new_with_plugin_root(
        cwd: PathBuf,
        sandbox: crate::sandbox::SandboxPolicy,
        plugin_root: PathBuf,
    ) -> Self {
        Self {
            cwd,
            shell: Arc::new(tokio::sync::Mutex::new(None)),
            sandbox,
            plugin_root: Some(plugin_root),
        }
    }
}

/// Parse `unsandboxed` from a JSON bool or a string ("true"/"false"/"1"/"0",
/// case-insensitive). Models occasionally emit `"unsandboxed": "true"` (string);
/// strict serde would reject the whole input as "parse failed", dropping the
/// tool call. Returns `None` for null, absent, or unrecognized values.
fn lenient_bool_value(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Deserialize `Option<bool>` accepting both a JSON bool and a lenient string
/// form (see [`lenient_bool_value`]). Null yields `None`; any other shape is a
/// hard error so a malformed value is surfaced rather than silently coerced.
pub(crate) fn lenient_bool_opt<'de, D>(d: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: serde_json::Value = serde::Deserialize::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    match lenient_bool_value(&v) {
        Some(b) => Ok(Some(b)),
        None => Err(serde::de::Error::custom(format!("expected bool, got {v}"))),
    }
}

/// Parse an unsigned integer from a JSON number or a numeric string. Models
/// occasionally emit `"timeout_secs": "600"` (string) alongside the well-formed
/// numeric form; strict serde would reject the whole input, dropping the tool
/// call and burning a turn. Returns `None` for non-numeric shapes.
fn lenient_unsigned_value(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Deserialize `Option<u64>` accepting both a JSON number and a numeric string
/// (see [`lenient_unsigned_value`]). Null yields `None`; any other shape is a
/// hard error so a malformed value is surfaced rather than silently coerced.
pub(crate) fn lenient_u64_opt<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: serde_json::Value = serde::Deserialize::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    match lenient_unsigned_value(&v) {
        Some(n) => Ok(Some(n)),
        None => Err(serde::de::Error::custom(format!("expected u64, got {v}"))),
    }
}

/// Deserialize `Option<usize>` accepting both a JSON number and a numeric
/// string. Saturates on overflow so a pathologically large value clamps to
/// `usize::MAX` instead of failing the whole tool call.
pub(crate) fn lenient_usize_opt<'de, D>(d: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: serde_json::Value = serde::Deserialize::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    match lenient_unsigned_value(&v) {
        Some(n) => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
        None => Err(serde::de::Error::custom(format!("expected usize, got {v}"))),
    }
}

#[derive(Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields)]
#[schemars(example = "bash_example")]
pub(crate) struct BashInput {
    /// Bash command to run (REQUIRED). This is the only required field.
    command: String,
    /// Working directory override (persists across subsequent calls).
    #[serde(default)]
    cwd: Option<String>,
    /// Kill the command after this many seconds (defaults to 120).
    #[serde(default, deserialize_with = "lenient_u64_opt")]
    timeout_secs: Option<u64>,
    /// Run outside the OS sandbox (macOS seatbelt). Default false: the command
    /// is confined to project root + temp dir writes, `.git` read-only, network
    /// denied, and runs without approval. Set true only when the command
    /// genuinely needs the outside (network, writes outside the project) — it
    /// then runs in a persistent shell with no confinement, gated by user
    /// approval.
    #[serde(default, deserialize_with = "lenient_bool_opt")]
    unsandboxed: Option<bool>,
    /// Keep only the first N lines of stdout/stderr. The command still runs to
    /// completion (no SIGPIPE) — prefer this over piping into `head`, which
    /// truncates the upstream pipe and can SIGPIPE-kill a long-running command.
    #[serde(default, deserialize_with = "lenient_usize_opt")]
    head_lines: Option<usize>,
    /// Keep only the last N lines of stdout/stderr. The command still runs to
    /// completion (no SIGPIPE) — prefer this over piping into `tail`.
    #[serde(default, deserialize_with = "lenient_usize_opt")]
    tail_lines: Option<usize>,
    /// When true, start the command in the background and return immediately
    /// with a shell id. Poll the shell id with `BashOutput` to read incremental
    /// output until the process exits. Mirrors Claude Code's `run_in_background`
    /// bash flag. Background shells are tracked in the process-global
    /// `background_shell` registry; they keep running across turns.
    #[serde(default, deserialize_with = "lenient_bool_opt")]
    run_in_background: Option<bool>,
}

/// Provide a concrete example for the JSON schema.
fn bash_example() -> serde_json::Value {
    serde_json::json!({
        "command": "ls -la",
        "cwd": "/path/to/project"
    })
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        super::BASH
    }
    fn description(&self) -> &str {
        "Run a bash command and return stdout. REQUIRED FIELD: `command` (string) — the bash command to execute. \
         Optional fields: `cwd`, `timeout_secs`, `unsandboxed`, `head_lines`, `tail_lines`, `run_in_background`. \
         Do NOT include any other fields (e.g., `description`, `name`, `id` are invalid). \
         Runs inside an OS sandbox by default (macOS seatbelt: writes confined to the project root and temp dir, \
         `.git` read-only, network disabled); each call is a one-shot `bash -c` — `cd`/`export` don't persist \
         across calls, so chain steps with `&&` or use the `cwd` parameter. For out-of-sandbox capabilities \
         (network, writing outside the project root) set `unsandboxed: true`, which after user approval runs in \
         a persistent shell session (cd/export persist across calls). Default timeout is 120s (override with \
         timeout_secs); on timeout or cancel the whole process group is terminated. stdout/stderr stream back to \
         the UI live. Do NOT pipe output to `head`/`tail` to reduce volume — that truncates the upstream pipe and \
         can SIGPIPE-kill a long-running command; use `head_lines`/`tail_lines` instead, which keep the selection \
         (first N, last N, or both with middle elided) while letting the command finish naturally."
    }
    fn requires_approval(&self, input: &serde_json::Value) -> bool {
        let unsandboxed = input
            .get("unsandboxed")
            .and_then(lenient_bool_value)
            .unwrap_or(false);
        if unsandboxed {
            return true;
        }
        // Cross-app automation (osascript / `tell application` / `open -a`)
        // escapes the FS+network confinement seatbelt enforces — seatbelt's
        // `(allow default)` base admits the Mach IPC Apple Events ride on.
        // Gate it on approval regardless of the `unsandboxed` flag so the model
        // cannot drive other apps without explicit, auditable user consent.
        // A YOLO session pre-authorizes every tool (like it does for
        // unsandboxed bash and write tools); outside YOLO this gate fires.
        if let Some(cmd) = input.get("command").and_then(serde_json::Value::as_str)
            && crate::sandbox::is_cross_app_automation(cmd)
        {
            return true;
        }
        // Sandboxed bash skips approval only where an OS sandbox actually
        // enforces confinement. On platforms without a backend, the default
        // path falls back to an unconstrained brush shell — gate it on
        // approval until a real backend (Linux bwrap / Windows) lands.
        !crate::sandbox::is_available()
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<BashInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        // Non-streaming entry: a throwaway sink whose chunks go unread.
        let (sink, _rx) = ToolOutputSink::channel("".into());
        self.run_streaming(input, cancel, sink, ctx, cx)
    }
    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        sink: ToolOutputSink,
        ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<BashInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let shell = self.shell.clone();
        let base_cwd = self.cwd.clone();
        let sandbox = self.sandbox.clone();
        let plugin_root = self.plugin_root.clone();
        let cwd_override = parsed.cwd.clone();
        let timeout = Duration::from_secs(parsed.timeout_secs.unwrap_or(BASH_DEFAULT_TIMEOUT_SECS));
        let command = parsed.command.clone();
        // YOLO mode forces the unsandboxed branch (DangerFullAccess): when the
        // owning thread has YOLO on, ignore the per-call `unsandboxed` flag and
        // always run via the persistent shell without seatbelt confinement.
        let yolo = ctx.yolo();
        let unsandboxed = parsed.unsandboxed.unwrap_or(false) || yolo;
        let head_lines = parsed.head_lines;
        let tail_lines = parsed.tail_lines;
        let run_in_background = parsed.run_in_background.unwrap_or(false);
        // The thread's in-process network proxy port, when the sandbox
        // network policy is `Restricted`. Injected into `wrap_command` so
        // the seatbelt narrows outbound to `localhost:<port>` and proxy env
        // vars point at the right address. `None` when the policy is
        // `Blocked` or `Unrestricted` (no proxy running).
        let proxy_port = ctx.network_proxy_port();
        // The model polls with BashOutput to collect incremental output.
        if run_in_background {
            let cwd_for_bg = cwd_override
                .as_deref()
                .map(|c| super::resolve_path(c, &base_cwd))
                .unwrap_or_else(|| base_cwd.clone());
            let timeout_bg = if parsed.timeout_secs.is_some() {
                Some(timeout)
            } else {
                None
            };
            let plugin_root_bg = plugin_root.clone();
            let sandbox_bg = sandbox.clone();
            // Spawn on the tokio runtime (tokio::process::Command needs an
            // active tokio reactor on the calling thread); bridge the result
            // back to the gpui executor via async_channel, mirroring monitor.
            let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
            let thread_id_bg = ctx.thread_id().to_string();
            let goal_id = ctx.goal_id().map(str::to_owned);
            let anchor_message_id = ctx.anchor_message_id().map(str::to_owned);
            crate::runtime::handle().spawn(async move {
                let result = super::background_shell::spawn(
                    command,
                    &thread_id_bg,
                    goal_id,
                    anchor_message_id,
                    &cwd_for_bg,
                    timeout_bg,
                    plugin_root_bg.as_deref(),
                    &sandbox_bg,
                    proxy_port,
                )
                .await
                .map(|shell_id| {
                    format!(
                        "Background shell started. shell_id: {shell_id}\n\
Poll with BashOutput (shell_id: \"{shell_id}\") to read incremental output \
until the process exits."
                    )
                });
                let _ = tx.send(result).await;
            });
            return cx.background_spawn(async move {
                rx.recv()
                    .await
                    .map_err(|_| "background shell cancelled".to_string())
                    .and_then(|r| r)
            });
        }
        bridge_tokio(cx, async move {
            if unsandboxed {
                // Approved escalation / YOLO: brush's persistent shell, no confinement.
                run_bash(
                    shell,
                    &command,
                    &base_cwd,
                    cwd_override.as_deref(),
                    &sandbox,
                    timeout,
                    cancel,
                    sink,
                    head_lines,
                    tail_lines,
                    plugin_root.as_deref(),
                )
                .await
            } else if crate::sandbox::is_available() {
                // Sandboxed default: seatbelt-wrapped subprocess, no approval.
                #[cfg(target_os = "macos")]
                {
                    run_sandboxed_bash(
                        &command,
                        &base_cwd,
                        cwd_override.as_deref(),
                        &sandbox,
                        proxy_port,
                        timeout,
                        cancel,
                        sink,
                        head_lines,
                        tail_lines,
                        plugin_root.as_deref(),
                    )
                    .await
                }
                #[cfg(not(target_os = "macos"))]
                {
                    unreachable!("is_available() true only on macos")
                }
            } else {
                // No OS sandbox on this platform: fall back to brush + warn.
                tracing::warn!(
                    "sandbox unavailable on this platform, running unsandboxed via brush"
                );
                run_bash(
                    shell,
                    &command,
                    &base_cwd,
                    cwd_override.as_deref(),
                    &sandbox,
                    timeout,
                    cancel,
                    sink,
                    head_lines,
                    tail_lines,
                    plugin_root.as_deref(),
                )
                .await
            }
        })
    }
}

/// Why the wait ended: natural exit, wall-clock timeout, or user cancellation.
enum Outcome {
    Ran(Result<ExecutionResult, brush_core::Error>),
    TimedOut,
    Cancelled,
}

/// Execute `command` in the persistent brush shell, enforce a wall-clock timeout
/// and cancellation, stream live output to `sink`, and return capped
/// stdout/stderr. On timeout/cancellation the spawned process groups are reaped
/// (SIGTERM → SIGKILL) so orphaned grandchildren cannot keep the pipes open.
///
/// `policy` confines the `cwd_override`: an override outside the writable set
/// or inside a protected subtree (`.git`) is rejected before brush runs. This
/// is the c5aefe4d escape — `cd` into a sibling worktree then git ops against
/// its `.git`. Direct writes outside the project root are NOT blocked here:
/// the unsandboxed path is the user-approved / YOLO escape route, and
/// constraining writes (not cwd) would defeat its purpose.
// too_many_arguments: the bash entry points pass the full subprocess-spawn +
// capture + cancel knob set down the call chain, and `run_sandboxed_bash`
// mirrors this list 1:1. Bundling into a config struct would diverge the two
// signatures and obscure the trivial arg-forward that routes unsandboxed →
// sandboxed, so the high count is structural, not a design smell.
#[allow(clippy::too_many_arguments)]
async fn run_bash(
    shell: Arc<tokio::sync::Mutex<Option<Shell>>>,
    command: &str,
    base_cwd: &Path,
    cwd_override: Option<&str>,
    policy: &crate::sandbox::SandboxPolicy,
    timeout: Duration,
    cancel: CancellationToken,
    sink: ToolOutputSink,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    plugin_root: Option<&Path>,
) -> Result<String, anyhow::Error> {
    // Reject a cwd override outside the writable set or inside a protected
    // subtree before brush executes it. `cd <sibling-worktree>` is the exact
    // c5aefe4d escape; blocking the cwd change blocks the subsequent git ops
    // against the foreign `.git`.
    if let Some(cwd) = cwd_override {
        let resolved = super::resolve_path(cwd, base_cwd);
        if !policy.is_write_allowed(&resolved) {
            return Err(anyhow::anyhow!(
                "Working directory is outside the sandbox writable area or falls into a protected path (`.git`): {}.\
                 To run commands outside the project root or in protected paths, set `unsandboxed: true` on the bash tool and get user approval.",
                resolved.display()
            ));
        }
    }
    // Two pipe pairs: external children and builtins both write here, the
    // spawn_blocking readers drain the read ends.
    let (out_r, out_w) = std::io::pipe()?;
    let (err_r, err_w) = std::io::pipe()?;
    let out_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
        "stdout",
    )));
    let err_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
        "stderr",
    )));
    // Clone the sink and buffers for each reader task; the originals stay here
    // so we can pull the final captured text once the readers finish.
    let out_sink = sink.clone();
    let err_sink = sink;
    let out_buf_task = out_buf.clone();
    let err_buf_task = err_buf.clone();
    let mut out_task =
        tokio::task::spawn_blocking(move || read_pipe(out_r, out_buf_task, out_sink));
    let mut err_task =
        tokio::task::spawn_blocking(move || read_pipe(err_r, err_buf_task, err_sink));

    // Hold the lock only across the race. `params` (and thus the brush-side pipe
    // write ends) drops at the end of this block, so once the child group is
    // killed the readers reach EOF.
    let ran: Outcome = {
        let mut guard = shell.lock().await;
        if guard.is_none() {
            let mut s = Shell::builder()
                .default_builtins(brush_builtins::BuiltinSet::BashMode)
                .build()
                .await
                .map_err(brush_err)?;
            s.set_working_dir(base_cwd).map_err(brush_err)?;
            // Inject the login shell's PATH so brush-spawned children find
            // Homebrew / toolchain binaries the GUI process env lacks (thread
            // `e5047fd2`: `gh` not found). The var must be exported: brush only
            // passes exported shell vars to child-process env, so an unexported
            // `set_env_global` would leave children on the launchd PATH.
            s.set_env_global(
                "PATH",
                exported_shell_var(crate::path_env::resolved_login_path().to_string()),
            )
            .map_err(brush_err)?;
            // Inject non-interactive editor/pager defaults (only when the
            // process env hasn't set them) so `git rebase --continue` / `git
            // log` do not open an interactive `$EDITOR` / pager and hang the
            // turn (thread 56ed5d5f msg308). Same export requirement as PATH:
            // `git` is an external child reading these from its env.
            for (k, v) in crate::sandbox::NONINTERACTIVE_ENV {
                if std::env::var_os(k).is_none() {
                    s.set_env_global(k, exported_shell_var((*v).to_string()))
                        .map_err(brush_err)?;
                }
            }
            // Inject CLAUDE_PLUGIN_ROOT so plugin-authored agent prompts
            // that reference ${CLAUDE_PLUGIN_ROOT} resolve correctly in
            // bash. Only set when the tool was constructed with a plugin
            // root (sub-agent spawned from a plugin agent definition).
            if let Some(root) = plugin_root {
                s.set_env_global(
                    "CLAUDE_PLUGIN_ROOT",
                    exported_shell_var(root.display().to_string()),
                )
                .map_err(brush_err)?;
            }
            *guard = Some(s);
        }
        if let Some(cwd) = cwd_override {
            // Resolve relative overrides against the thread cwd explicitly,
            // rather than relying on brush's internal resolution — the model
            // may pass a relative path meaning "relative to thread cwd", and
            // `resolve_path` makes that absolute before brush sees it.
            let resolved = super::resolve_path(cwd, base_cwd);
            guard
                .as_mut()
                .expect("shell initialized")
                .set_working_dir(&resolved)
                .map_err(brush_err)?;
        }
        let sh = guard.as_mut().expect("shell initialized");
        let mut params = sh.default_exec_params();
        // Each external command leads its own process group so the cancel/timeout
        // path can reap the whole tree with `kill(-pgid)`, matching the prior
        // `setsid` harness. brush defaults to `SameProcessGroup` when job control
        // is off, which would make the group-id kill miss.
        params.process_group_policy = brush_core::ProcessGroupPolicy::NewProcessGroup;
        params.set_fd(OpenFiles::STDIN_FD, openfiles::null().map_err(brush_err)?);
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(out_w));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(err_w));
        let source = SourceInfo::default();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            r = sh.run_string(command, &source, &params) => Outcome::Ran(r),
        }
    };

    // brush does not kill_on_drop, so a cancelled/timed-out run leaves the
    // spawned groups orphaned. Reap them now that the `run_string` borrow is
    // released.
    if matches!(ran, Outcome::Cancelled | Outcome::TimedOut) {
        let mut guard = shell.lock().await;
        let sh = guard.as_mut().expect("shell initialized");
        kill_jobs(sh, libc::SIGTERM);
        tokio::time::sleep(Duration::from_millis(CANCELLATION_GRACE_MS)).await;
        kill_jobs(sh, libc::SIGKILL);
        let _ = sh.jobs_mut().poll();
        sh.jobs_mut().jobs.clear();
    }

    // Drain readers within the IO deadline, then extract the captured text.
    drain(&mut out_task).await;
    drain(&mut err_task).await;
    let (out_str, _out_trunc, out_dropped, out_artifact) =
        out_buf.lock().expect("capture buffer lock poisoned").take();
    let (err_str, _err_trunc, err_dropped, err_artifact) =
        err_buf.lock().expect("capture buffer lock poisoned").take();

    let out_str = select_lines(&out_str, head_lines, tail_lines);
    let err_str = select_lines(&err_str, head_lines, tail_lines);
    format_result(
        ran,
        out_str,
        out_dropped,
        out_artifact,
        err_str,
        err_dropped,
        err_artifact,
    )
}

/// Format the outcome: success returns stdout (or stderr when stdout is empty);
/// non-zero exit / brush error / timeout / cancel returns an error carrying both
/// streams.
fn format_result(
    ran: Outcome,
    stdout: String,
    stdout_dropped: usize,
    stdout_artifact: Option<std::path::PathBuf>,
    stderr: String,
    stderr_dropped: usize,
    stderr_artifact: Option<std::path::PathBuf>,
) -> Result<String, anyhow::Error> {
    // `truncated == dropped > 0` is the `CaptureBuffer` invariant, so the bool
    // is derivable and not passed separately.
    let stdout = render_stream(&stdout, stdout_dropped, &stdout_artifact);
    let stderr = render_stream(&stderr, stderr_dropped, &stderr_artifact);
    match ran {
        Outcome::Ran(Ok(er)) => {
            if er.exit_code.is_success() {
                let combined = if stdout.is_empty() { stderr } else { stdout };
                Ok(combined)
            } else {
                let code = exit_code_num(er.exit_code);
                Err(anyhow::anyhow!(
                    "bash exit code {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ))
            }
        }
        Outcome::Ran(Err(e)) => Err(anyhow::anyhow!(
            "bash execution failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            brush_err(e)
        )),
        Outcome::TimedOut => Err(anyhow::anyhow!(
            "bash timed out (process group killed)\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
        Outcome::Cancelled => Err(anyhow::anyhow!(
            "bash cancelled\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
    }
}

/// Render one captured stream with the truncation advisory. When the full
/// stream was tee'd to an artifact, the notice points at it so the model can
/// read the complete output instead of re-running a narrower command blind.
fn render_stream(text: &str, dropped: usize, artifact: &Option<std::path::PathBuf>) -> String {
    let hint = match artifact {
        Some(p) => format!("full output: {}", p.display()),
        None => BASH_TRUNCATION_HINT.to_string(),
    };
    crate::tools::TruncatedText::new(text, BASH_OUTPUT_MAX_BYTES, dropped).render(&hint)
}

/// Numeric exit code for the model-facing message. brush maps well-known codes
/// to named variants; `Custom(u8)` carries anything else.
fn exit_code_num(code: ExecutionExitCode) -> u8 {
    match code {
        ExecutionExitCode::Success => 0,
        ExecutionExitCode::GeneralError => 1,
        ExecutionExitCode::InvalidUsage => 2,
        ExecutionExitCode::Unimplemented => 99,
        ExecutionExitCode::CannotExecute => 126,
        ExecutionExitCode::NotFound => 127,
        ExecutionExitCode::Interrupted => 130,
        ExecutionExitCode::BrokenPipe => 141,
        ExecutionExitCode::Custom(c) => c,
    }
}

/// Drain a capped read task within the IO drain deadline; abort it if the
/// deadline is missed (a grandchild holding the pipe open would otherwise pin
/// the blocking thread).
async fn drain(task: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_millis(IO_DRAIN_TIMEOUT_MS), &mut *task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// Blocking reader: appends bytes into a [`CaptureBuffer`] and forwards complete
/// lines to the live `sink`. Runs on a `spawn_blocking` thread so the async
/// runtime is never stalled by a slow pipe.
fn read_pipe(
    mut r: std::io::PipeReader,
    buf: Arc<std::sync::Mutex<CaptureBuffer>>,
    sink: ToolOutputSink,
) {
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = &chunk[..n];
                if let Ok(mut b) = buf.lock() {
                    b.push(data);
                }
                carry.extend_from_slice(data);
                while let Some(i) = carry.iter().position(|&b| b == b'\n') {
                    let rest = carry.split_off(i + 1);
                    let line = std::mem::take(&mut carry);
                    sink.try_emit(&String::from_utf8_lossy(&line));
                    carry = rest;
                }
            }
        }
    }
    if !carry.is_empty() {
        sink.try_emit(&String::from_utf8_lossy(&carry));
    }
}

/// Byte-capped buffer: appends until `max`, then drops the overflowing text
/// while still tallying the total bytes seen. The total lets the truncation
/// notice tell the model how much it is missing, so it knows to narrow the
/// command rather than guess at the truncated tail.
///
/// Once the cap is hit, the full stream is tee'd into a temp-file artifact
/// (buffered head + everything that follows) and the truncation notice
/// carries its path — the model can `read_file`/`grep` the complete output
/// instead of re-running a narrower command blind.
struct CaptureBuffer {
    buf: Vec<u8>,
    max: usize,
    truncated: bool,
    /// Bytes seen after the cap was hit; reported in the truncation notice.
    /// Only accrued once `truncated` is set, so this is the dropped tail size.
    dropped: usize,
    /// Stream label (`stdout`/`stderr`) used in the artifact filename.
    stream: &'static str,
    artifact: Option<std::fs::File>,
    artifact_path: Option<std::path::PathBuf>,
}

impl CaptureBuffer {
    fn new(max: usize, stream: &'static str) -> Self {
        Self {
            buf: Vec::with_capacity(8 * 1024),
            max,
            truncated: false,
            dropped: 0,
            stream,
            artifact: None,
            artifact_path: None,
        }
    }

    fn push(&mut self, data: &[u8]) {
        if self.truncated {
            self.dropped = self.dropped.saturating_add(data.len());
            self.tee_artifact(data);
            return;
        }
        let remaining = self.max.saturating_sub(self.buf.len());
        if data.len() <= remaining {
            self.buf.extend_from_slice(data);
        } else {
            self.buf.extend_from_slice(&data[..remaining]);
            self.dropped = self.dropped.saturating_add(data.len() - remaining);
            self.truncated = true;
            self.start_artifact();
            self.tee_artifact(&data[remaining..]);
        }
    }

    /// Open the artifact with everything buffered so far as its head. Best
    /// effort: a filesystem failure drops the artifact, never the capture.
    fn start_artifact(&mut self) {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!(
            "manox-bash-{}-{}.log",
            uuid::Uuid::new_v4(),
            self.stream
        ));
        if let Ok(mut f) = std::fs::File::create(&path)
            && f.write_all(&self.buf).is_ok()
        {
            self.artifact_path = Some(path);
            self.artifact = Some(f);
        }
    }

    fn tee_artifact(&mut self, data: &[u8]) {
        use std::io::Write as _;
        if let Some(f) = self.artifact.as_mut() {
            let _ = f.write_all(data);
        }
    }

    /// Returns `(text, truncated, dropped_bytes, artifact_path)`.
    fn take(&mut self) -> (String, bool, usize, Option<std::path::PathBuf>) {
        let data = std::mem::take(&mut self.buf);
        let dropped = std::mem::take(&mut self.dropped);
        (
            String::from_utf8_lossy(&data).into_owned(),
            self.truncated,
            dropped,
            self.artifact_path.take(),
        )
    }
}

/// Return the requested slice of `text`: first `head` lines, last `tail`
/// lines, or both with the elided middle collapsed into a marker. The command
/// has already run to completion (capture is post-hoc), so unlike a `| head`
/// pipe the upstream process never sees SIGPIPE. `None`/`None` returns the
/// full text unchanged — the byte cap ([`CaptureBuffer`] / [`TruncatedText`])
/// still applies downstream.
///
/// When both bounds are set and the text has more than `head + tail` lines, the
/// middle is replaced with `... (N lines elided) ...` so the model knows lines
/// were dropped and can widen the selection. `split_inclusive('\n')` keeps the
/// trailing newline with each line so re-joining reproduces the original
/// byte layout, including a final unterminated line.
fn select_lines(text: &str, head: Option<usize>, tail: Option<usize>) -> String {
    match (head, tail) {
        (None, None) => text.to_string(),
        (Some(h), None) => text.split_inclusive('\n').take(h).collect::<String>(),
        (None, Some(t)) => {
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            let start = lines.len().saturating_sub(t);
            lines[start..].concat()
        }
        (Some(h), Some(t)) => {
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            if lines.len() <= h + t {
                text.to_string()
            } else {
                let head_part: String = lines[..h].concat();
                let tail_start = lines.len() - t;
                let tail_part: String = lines[tail_start..].concat();
                let elided = lines.len() - h - t;
                format!(
                    "{head_part}... ({elided} lines elided, widen head_lines/tail_lines to see them) ...\n{tail_part}"
                )
            }
        }
    }
}

/// Why a sandboxed wait ended: child exited, spawn failed, wall-clock
/// timeout, or user cancellation.
#[cfg(target_os = "macos")]
enum SandboxOutcome {
    Exited(std::process::ExitStatus),
    SpawnFailed(std::io::Error),
    TimedOut,
    Cancelled,
}

/// Format a sandboxed run. Mirrors [`format_result`] but over `ExitStatus`
/// instead of brush's `ExecutionResult`; the truncation rendering is shared
/// via [`render_stream`].
#[cfg(target_os = "macos")]
fn format_sandboxed_result(
    ran: SandboxOutcome,
    stdout: String,
    stdout_dropped: usize,
    stdout_artifact: Option<std::path::PathBuf>,
    stderr: String,
    stderr_dropped: usize,
    stderr_artifact: Option<std::path::PathBuf>,
) -> Result<String, anyhow::Error> {
    let stdout = render_stream(&stdout, stdout_dropped, &stdout_artifact);
    let stderr = render_stream(&stderr, stderr_dropped, &stderr_artifact);
    match ran {
        SandboxOutcome::Exited(status) => {
            if status.success() {
                let combined = if stdout.is_empty() { stderr } else { stdout };
                Ok(combined)
            } else {
                let code = status.code().unwrap_or(-1);
                Err(anyhow::anyhow!(
                    "bash exit code {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ))
            }
        }
        SandboxOutcome::SpawnFailed(e) => Err(anyhow::anyhow!(
            "bash spawn failed: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
        SandboxOutcome::TimedOut => Err(anyhow::anyhow!(
            "bash timed out (process group killed)\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
        SandboxOutcome::Cancelled => Err(anyhow::anyhow!(
            "bash cancelled\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
    }
}

/// Async chunk reader: appends bytes into a [`CaptureBuffer`] and forwards
/// complete lines to the live `sink`, mirroring the sync [`read_pipe`] but for
/// a tokio pipe. Reads in fixed 8 KiB chunks (not `read_until`) so a
/// newline-less binary stream can't grow an unbounded line buffer or emit a
/// single huge chunk to the sink. EOFs when the child's stdout/stderr close.
#[cfg(target_os = "macos")]
async fn read_pipe_async<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    buf: Arc<std::sync::Mutex<CaptureBuffer>>,
    sink: ToolOutputSink,
) {
    use tokio::io::AsyncReadExt;
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = &chunk[..n];
                if let Ok(mut b) = buf.lock() {
                    b.push(data);
                }
                carry.extend_from_slice(data);
                while let Some(i) = carry.iter().position(|&b| b == b'\n') {
                    let rest = carry.split_off(i + 1);
                    let line = std::mem::take(&mut carry);
                    sink.try_emit(&String::from_utf8_lossy(&line));
                    carry = rest;
                }
            }
        }
    }
    if !carry.is_empty() {
        sink.try_emit(&String::from_utf8_lossy(&carry));
    }
}

/// Execute `command` in a seatbelt-sandboxed `sandbox-exec` subprocess: write
/// to project root + temp dir only, `.git` read-only, network denied. Mirrors
/// [`run_bash`]'s capture/timeout/cancel structure but over a one-shot
/// `tokio::process::Command` rather than brush's persistent shell.
#[cfg(target_os = "macos")]
// too_many_arguments: mirrors `run_bash`'s parameter list 1:1 (the sandboxed
// path swaps brush's persistent shell for a one-shot `sandbox-exec` Command
// but keeps the same knobs); see `run_bash` for why a config struct is worse.
#[allow(clippy::too_many_arguments)]
async fn run_sandboxed_bash(
    command: &str,
    base_cwd: &Path,
    cwd_override: Option<&str>,
    policy: &crate::sandbox::SandboxPolicy,
    proxy_port: Option<u16>,
    timeout: Duration,
    cancel: CancellationToken,
    sink: ToolOutputSink,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    plugin_root: Option<&Path>,
) -> Result<String, anyhow::Error> {
    use std::process::Stdio;
    use tokio::process::Child;
    let cwd = cwd_override
        .map(|c| super::resolve_path(c, base_cwd))
        .unwrap_or_else(|| base_cwd.to_path_buf());
    let mut cmd = policy.wrap_command(command, &cwd, proxy_port);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    // Inject CLAUDE_PLUGIN_ROOT so plugin-authored agent prompts that
    // reference ${CLAUDE_PLUGIN_ROOT} resolve in sandboxed bash too.
    if let Some(root) = plugin_root {
        cmd.env("CLAUDE_PLUGIN_ROOT", root);
    }

    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return format_sandboxed_result(
                SandboxOutcome::SpawnFailed(e),
                String::new(),
                0,
                None,
                String::new(),
                0,
                None,
            );
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let out_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
        "stdout",
    )));
    let err_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
        "stderr",
    )));
    let out_sink = sink.clone();
    let err_sink = sink;
    let out_buf_task = out_buf.clone();
    let err_buf_task = err_buf.clone();

    let mut out_task = tokio::task::spawn(read_pipe_async(stdout, out_buf_task, out_sink));
    let mut err_task = tokio::task::spawn(read_pipe_async(stderr, err_buf_task, err_sink));

    // Race the wait against the wall-clock timeout and user cancellation. The
    // `child.wait()` future is inline in the select! (not pinned outside), so
    // when cancel/timeout wins it is dropped — releasing the `&mut child`
    // borrow so we can `start_kill` + reap on the next line.
    let ran = tokio::select! {
        status = child.wait() => match status {
            Ok(s) => SandboxOutcome::Exited(s),
            Err(e) => SandboxOutcome::SpawnFailed(e),
        },
        _ = cancel.cancelled() => SandboxOutcome::Cancelled,
        _ = tokio::time::sleep(timeout) => SandboxOutcome::TimedOut,
    };
    if matches!(ran, SandboxOutcome::Cancelled | SandboxOutcome::TimedOut) {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    // Drain readers within the IO deadline; grandchildren holding the pipes
    // open would otherwise stall the join. The `join!` inside the timeout polls
    // each handle at most once; on a missed deadline we abort instead of polling
    // again — a `JoinHandle` polled after `Complete` panics, which was the
    // sandboxed-bash crash when the readers finished before the deadline.
    if tokio::time::timeout(Duration::from_millis(IO_DRAIN_TIMEOUT_MS), async {
        let _ = tokio::join!(&mut out_task, &mut err_task);
    })
    .await
    .is_err()
    {
        out_task.abort();
        err_task.abort();
    }

    let (out_str, out_dropped, out_artifact) = {
        let mut b = out_buf.lock().expect("capture buffer lock poisoned");
        let (t, _, d, a) = b.take();
        (t, d, a)
    };
    let (err_str, err_dropped, err_artifact) = {
        let mut b = err_buf.lock().expect("capture buffer lock poisoned");
        let (t, _, d, a) = b.take();
        (t, d, a)
    };

    let out_str = select_lines(&out_str, head_lines, tail_lines);
    let err_str = select_lines(&err_str, head_lines, tail_lines);
    format_sandboxed_result(
        ran,
        out_str,
        out_dropped,
        out_artifact,
        err_str,
        err_dropped,
        err_artifact,
    )
}

/// Signal every tracked job's process group. With `NewProcessGroup` each
/// external command leads its own group (pgid == child pid), so `kill(-pgid)`
/// reaches the whole tree — equivalent to the prior `setsid` + `kill(-pid)`.
#[cfg(unix)]
fn kill_jobs(shell: &Shell, sig: i32) {
    for job in &shell.jobs().jobs {
        if let Some(pgid) = job.process_group_id() {
            // Best-effort: the group may already be gone by the time we escalate.
            unsafe {
                let _ = libc::kill(-pgid, sig);
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_jobs(_shell: &Shell, _sig: i32) {}

/// Adapt a brush error into an anyhow error for the model-facing string.
fn brush_err(e: brush_core::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fresh_shell() -> Arc<tokio::sync::Mutex<Option<Shell>>> {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    fn null_sink() -> ToolOutputSink {
        let (sink, _rx) = ToolOutputSink::channel("".into());
        sink
    }

    fn policy() -> crate::sandbox::SandboxPolicy {
        crate::sandbox::SandboxPolicy::for_project(&PathBuf::from("."))
    }

    // These exercise `run_bash` directly (no gpui App) so the brush execution,
    // timeout escalation, and cancellation paths are covered.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_success_returns_stdout() {
        let out = run_bash(
            fresh_shell(),
            "printf hello",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(out, "hello");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sandboxed_bash_fast_exit_does_not_panic() {
        // Regression: `run_sandboxed_bash` used to `join!` the reader handles
        // inside the drain timeout and then `drain()` them again — polling a
        // completed JoinHandle panics ("JoinHandle polled after completion").
        // A command that exits immediately makes the readers finish before the
        // drain deadline, which was the exact crash repro. The fixed path
        // aborts on a missed deadline instead of re-polling, so a fast exit
        // must complete without panicking.
        //
        // `sandbox-exec` cannot apply its profile when the test process is
        // itself already seatbelt-confined (e.g. running under another
        // sandbox or in certain CI contexts). The error message contains
        // "Operation not permitted"; in that case the regression is not
        // exercisable, so skip rather than fail.
        let result = run_sandboxed_bash(
            "true",
            &PathBuf::from("."),
            None,
            &policy(),
            None,
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await;
        match result {
            Ok(out) => assert!(out.is_empty(), "expected empty output, got: {out}"),
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("Operation not permitted") || msg.contains("sandbox_apply") {
                    eprintln!("skipped: sandbox-exec unavailable in this process context");
                    return;
                }
                panic!("fast exit must not panic and must report Ok: {msg}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_nonzero_exit_is_error_with_code() {
        let err = run_bash(
            fresh_shell(),
            "exit 7",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exit code 7"), "got: {msg}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_exports_login_path_to_child_env() {
        // Regression for thread `ed3391e6`: brush clears child-process env and
        // rebuilds it from exported shell vars only (`brush-core` `commands.rs`:
        // `env_clear()` + `iter_exported()`). `set_env_global` on a plain
        // `ShellVariable::new` set a non-exported PATH, so the shell var was
        // correct (`echo $PATH` showed the login PATH, `brew` ran via brush's own
        // lookup) but external children got NO path var — `/usr/bin/which brew` /
        // `printenv PATH` saw nothing and reported "not found". Marking the var
        // exported makes an external child reading its env see the login PATH.
        let out = run_bash(
            fresh_shell(),
            "printenv PATH",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await
        .expect("printenv must succeed when PATH is exported");
        assert_eq!(
            out.trim(),
            crate::path_env::resolved_login_path(),
            "child must see the login PATH, not an unset/launchd PATH"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_timeout_kills_command() {
        let start = std::time::Instant::now();
        let err = run_bash(
            fresh_shell(),
            "sleep 30",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(1),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        // The 1s timeout must reap the process, not wait out the full 30s sleep.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "elapsed {:?}",
            start.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_cancel_aborts_command() {
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            cancel2.cancel();
        });
        let err = run_bash(
            fresh_shell(),
            "sleep 30",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(60),
            cancel,
            null_sink(),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cancelled"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_timeout_reaps_orphaned_grandchild() {
        // `sleep 30 & wait` backgrounds a job the foreground `wait` is blocked on.
        // The timeout's group-kill must reach it; the turn must not block past
        // the timeout waiting on a pipe the grandchild keeps open.
        let start = std::time::Instant::now();
        let _ = run_bash(
            fresh_shell(),
            "sleep 30 & wait",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(1),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_persistent_session_preserves_cwd() {
        // `cd` in one call must persist to the next: the defining property of
        // the brush-backed persistent shell.
        let shell = fresh_shell();
        let _ = run_bash(
            shell.clone(),
            "cd /tmp",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let out = run_bash(
            shell,
            "pwd",
            &PathBuf::from("."),
            None,
            &policy(),
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(out.trim_end(), "/tmp");
    }

    #[test]
    fn capture_buffer_caps_and_tracks_dropped_total() {
        let mut buf = CaptureBuffer::new(64 * 1024, "stdout");
        // First 64 KiB fills the cap exactly without truncation.
        buf.push(&vec![b'x'; 64 * 1024]);
        let (_, truncated, dropped, artifact) = buf.take();
        assert!(!truncated, "exactly at cap is not truncated");
        assert_eq!(dropped, 0);
        assert!(artifact.is_none(), "no artifact without truncation");

        // Now overflow: cap + 10 KiB more. The tail is dropped but tallied.
        let mut buf = CaptureBuffer::new(64 * 1024, "stdout");
        buf.push(&vec![b'x'; 64 * 1024]);
        buf.push(&vec![b'y'; 10 * 1024]);
        let (text, truncated, dropped, artifact) = buf.take();
        assert!(truncated);
        assert_eq!(dropped, 10 * 1024);
        assert!(!text.contains('y'), "dropped tail must not appear in text");
        // The artifact preserves the full stream: 64 KiB head + dropped tail.
        let path = artifact.expect("overflow tees the full stream to an artifact");
        let full = std::fs::read_to_string(&path).expect("read artifact");
        assert_eq!(full.len(), 64 * 1024 + 10 * 1024);
        assert!(full.ends_with(&"y".repeat(10 * 1024)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncation_notice_points_at_artifact_when_present() {
        let path = std::path::PathBuf::from("/tmp/manox-bash-test-stdout.log");
        let out = render_stream("head", 10 * 1024, &Some(path));
        assert!(out.contains("full output: /tmp/manox-bash-test-stdout.log"));
        let out = render_stream("head", 0, &None);
        assert!(!out.contains("full output:"), "no artifact, no pointer");
    }

    #[test]
    fn truncation_notice_is_prefixed_advisory_and_reports_total() {
        // 64 KiB cap + 12 KiB dropped → 76 KiB total reported.
        let note =
            crate::tools::TruncatedText::new("leftover line", BASH_OUTPUT_MAX_BYTES, 12 * 1024)
                .render(BASH_TRUNCATION_HINT);
        assert!(
            note.starts_with('⚠'),
            "notice must be prefixed so the model sees it first: {note}"
        );
        assert!(
            note.contains("narrower command"),
            "must advise narrowing: {note}"
        );
        assert!(
            note.contains(&format!(
                "{} bytes total",
                BASH_OUTPUT_MAX_BYTES + 12 * 1024
            )),
            "must report total bytes: {note}"
        );
        assert!(
            note.contains("leftover line"),
            "truncated text must still be present: {note}"
        );
        // The notice is prefixed, so the truncated text comes after the warning.
        let warning_idx = note.find('⚠').unwrap();
        let text_idx = note.find("leftover line").unwrap();
        assert!(warning_idx < text_idx);
    }

    #[test]
    fn truncation_notice_absent_when_not_truncated() {
        let s = crate::tools::TruncatedText::new("plain output", BASH_OUTPUT_MAX_BYTES, 0)
            .render(BASH_TRUNCATION_HINT);
        assert_eq!(s, "plain output");
    }

    #[test]
    fn lenient_bool_value_accepts_bool_and_string_forms() {
        assert_eq!(lenient_bool_value(&serde_json::json!(true)), Some(true));
        assert_eq!(lenient_bool_value(&serde_json::json!(false)), Some(false));
        assert_eq!(lenient_bool_value(&serde_json::json!("true")), Some(true));
        assert_eq!(lenient_bool_value(&serde_json::json!("FALSE")), Some(false));
        assert_eq!(lenient_bool_value(&serde_json::json!("1")), Some(true));
        assert_eq!(lenient_bool_value(&serde_json::json!("0")), Some(false));
        assert_eq!(lenient_bool_value(&serde_json::json!(null)), None);
        assert_eq!(lenient_bool_value(&serde_json::json!("maybe")), None);
        assert_eq!(lenient_bool_value(&serde_json::json!(42)), None);
    }

    #[test]
    fn bash_input_parses_string_unsandboxed() {
        // Models sometimes emit `"unsandboxed": "true"` (string). Strict serde
        // would reject the whole input; the lenient deserializer accepts it.
        let parsed: BashInput =
            serde_json::from_value(serde_json::json!({"command":"ls","unsandboxed":"true"}))
                .expect("string \"true\" must parse");
        assert_eq!(parsed.unsandboxed, Some(true));

        let parsed: BashInput =
            serde_json::from_value(serde_json::json!({"command":"ls","unsandboxed":"false"}))
                .expect("string \"false\" must parse");
        assert_eq!(parsed.unsandboxed, Some(false));

        let parsed: BashInput =
            serde_json::from_value(serde_json::json!({"command":"ls","unsandboxed":true}))
                .expect("bool true must parse");
        assert_eq!(parsed.unsandboxed, Some(true));

        let parsed: BashInput = serde_json::from_value(serde_json::json!({"command":"ls"}))
            .expect("absent unsandboxed must parse");
        assert_eq!(parsed.unsandboxed, None);
    }

    #[test]
    fn bash_input_rejects_malformed_unsandboxed() {
        let err = serde_json::from_value::<BashInput>(serde_json::json!({
            "command":"ls","unsandboxed":"maybe"
        }))
        .unwrap_err();
        assert!(
            err.to_string().contains("expected bool"),
            "malformed value must surface a clear error: {err}"
        );
    }

    // ─── requires_approval: cross-app automation gating ─────────────────────

    fn tool() -> BashTool {
        BashTool::new(
            PathBuf::from("."),
            crate::sandbox::SandboxPolicy::for_project(&PathBuf::from(".")),
        )
    }

    #[test]
    fn requires_approval_flags_cross_app_commands() {
        // Cross-app automation drives other apps via Apple Events; even a
        // sandboxed command must be approval-gated, on every platform.
        let t = tool();
        assert!(t.requires_approval(&serde_json::json!({
            "command": "osascript -e 'tell application \"Finder\" to quit'"
        })));
        assert!(t.requires_approval(&serde_json::json!({
            "command": "open -a 'Visual Studio Code' ."
        })));
    }

    #[test]
    fn requires_approval_unsandboxed_still_true() {
        let t = tool();
        assert!(t.requires_approval(&serde_json::json!({
            "command": "ls", "unsandboxed": true
        })));
    }

    #[test]
    fn requires_approval_allows_normal_sandboxed_on_macos() {
        // A plain sandboxed command is approval-free exactly where an OS
        // sandbox backend enforces confinement (macOS). On platforms without a
        // backend the fallback brush shell is gated, so the same command needs
        // approval there.
        let t = tool();
        let decided = t.requires_approval(&serde_json::json!({"command": "ls -la"}));
        if crate::sandbox::is_available() {
            assert!(!decided, "sandboxed command must skip approval on macOS");
        } else {
            assert!(
                decided,
                "sandboxed command needs approval without a backend"
            );
        }
    }

    // ─── select_lines: head/tail selection without SIGPIPE ─────────────────

    #[test]
    fn select_lines_none_returns_full_text() {
        let s = "line1\nline2\nline3\n";
        assert_eq!(select_lines(s, None, None), s);
    }

    #[test]
    fn select_lines_head_only_keeps_first_n() {
        assert_eq!(select_lines("a\nb\nc\nd\n", Some(2), None), "a\nb\n");
        // Fewer lines than requested: returns all of them, no padding.
        assert_eq!(select_lines("a\nb\n", Some(5), None), "a\nb\n");
    }

    #[test]
    fn select_lines_tail_only_keeps_last_n() {
        assert_eq!(select_lines("a\nb\nc\nd\n", None, Some(2)), "c\nd\n");
        assert_eq!(select_lines("a\nb\n", None, Some(5)), "a\nb\n");
    }

    #[test]
    fn select_lines_head_and_tail_elides_middle() {
        let out = select_lines("a\nb\nc\nd\ne\n", Some(1), Some(1));
        assert!(out.starts_with("a\n"), "head kept first: {out}");
        assert!(out.ends_with("e\n"), "tail kept last: {out}");
        assert!(
            out.contains("3 lines elided"),
            "elision count reported: {out}"
        );
    }

    #[test]
    fn select_lines_head_and_tail_no_elision_when_short_enough() {
        // 3 lines, head=1 + tail=1 = 2 < 3 → elide. head=2 + tail=2 = 4 ≥ 3 →
        // no elision, full text returned verbatim.
        assert!(select_lines("a\nb\nc\n", Some(1), Some(1)).contains("elided"));
        assert_eq!(select_lines("a\nb\nc\n", Some(2), Some(2)), "a\nb\nc\n");
    }

    #[test]
    fn select_lines_handles_unterminated_final_line() {
        // A final line without a trailing newline is preserved as one line.
        assert_eq!(select_lines("a\nb\nca", Some(1), None), "a\n");
        assert_eq!(select_lines("a\nb\nca", None, Some(1)), "ca");
    }

    #[test]
    fn bash_input_parses_numeric_string_timeout() {
        // Models occasionally emit `"timeout_secs": "600"` (string); strict serde
        // would drop the whole call. The lenient deserializer accepts both forms.
        let v = serde_json::json!({"command": "sleep 1", "timeout_secs": "600"});
        let p: BashInput = serde_json::from_value(v).unwrap();
        assert_eq!(p.timeout_secs, Some(600));
    }

    #[test]
    fn bash_input_parses_numeric_string_head_tail() {
        let v = serde_json::json!({"command": "true", "head_lines": "3", "tail_lines": "5"});
        let p: BashInput = serde_json::from_value(v).unwrap();
        assert_eq!(p.head_lines, Some(3));
        assert_eq!(p.tail_lines, Some(5));
    }

    #[test]
    fn bash_input_rejects_non_numeric_timeout() {
        let v = serde_json::json!({"command": "true", "timeout_secs": "soon"});
        let err = serde_json::from_value::<BashInput>(v).unwrap_err();
        assert!(err.to_string().contains("expected u64"), "{err}");
    }
}
