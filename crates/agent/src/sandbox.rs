//! OS-level sandbox for the pi-path `bash` tool: macOS seatbelt
//! (`sandbox-exec`) wrapping one-shot commands.
//!
//! The seatbelt renders a per-call file-effect profile from the effective
//! `PermissionMode`: `read-only` denies all file writes (only required sinks
//! like `/dev/null`); `workspace-write` allows the shared `writable_roots`;
//! `danger-full-access` skips the seatbelt entirely (the unsandboxed backend).
//! Mirrors `~/projects/github/deepseek-harness` `dsh-sandbox-local`.
//!
//! Network and `.git` protection are outside the mode vocabulary: the
//! `(allow default)` base admits all network, and `workspace-write` allows any
//! path under a writable root (including `.git`). Reads and process execution
//! stay unrestricted in every mode.
//!
//! The kernel `BashOperations` trait stays untouched (layering red line: no
//! domain field in the kernel). The per-call effective mode reaches the
//! renderer via a host-injected `mode_resolver` closure, mirroring the
//! existing `force_unsandboxed` injection pattern.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi::env::{CommandResult, ExecutionError};
use pi::tools::bash::{BashExecRequest, BashOperations};
use pi_extensions::sandbox::{PermissionMode, writable_roots};
use tokio_util::sync::CancellationToken;

// The one canonicalization home: the seatbelt and the fs fence share
// `pi_extensions::sandbox`'s `..`-folding implementation so a traversal
// cannot survive into a classified path. Re-exported here for the host
// callers that still reach it as `crate::sandbox::canonicalize_best_effort`.
pub use pi_extensions::sandbox::canonicalize_best_effort;

/// Default wall-clock limit for a one-shot command (mirrors the bash tool's
/// own default).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Confinement policy for one sandboxed invocation: the workspace root whose
/// `writable_roots()` the `workspace-write` profile allows, plus the worktree
/// anchor (when the policy is worktree-scoped) for callers that key off it.
/// The effective mode is supplied per call by the `mode_resolver` on
/// [`SandboxedBashOperations`], not held here.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    workspace_root: PathBuf,
    worktree_anchor: Option<PathBuf>,
}

impl SandboxPolicy {
    /// Build the project policy for `project_root`. The root is canonicalized
    /// best-effort so the seatbelt `subpath` matching (which resolves symlinks)
    /// compares against real paths — the temp dir is a symlink to
    /// `/private/var/...` on macOS.
    pub fn for_project(project_root: &Path) -> Self {
        Self {
            workspace_root: canonicalize_best_effort(project_root),
            worktree_anchor: None,
        }
    }

    /// Policy for a worktree-isolated session: the workspace root is the
    /// worktree itself, so `workspace-write` allows the worktree (plus the
    /// shared temp areas). A worktree is an approved isolation context.
    pub fn for_worktree(worktree_path: &Path) -> Self {
        let worktree = canonicalize_best_effort(worktree_path);
        Self {
            workspace_root: worktree.clone(),
            worktree_anchor: Some(worktree),
        }
    }

    /// The active worktree root when this policy is worktree-scoped.
    pub fn worktree_anchor(&self) -> Option<&Path> {
        self.worktree_anchor.as_deref()
    }

    /// Render a seatbelt (`.sbpl`) policy string for the effective `mode`.
    /// Denylist base (`(allow default)`) with an allowlist over writes;
    /// network is unrestricted (the mode vocabulary governs file effects only,
    /// not network or process visibility). `read-only` admits no writable
    /// roots; `workspace-write` admits the shared `writable_roots()`.
    /// `danger-full-access` is never rendered — backend selection skips the
    /// seatbelt for it.
    pub fn render_seatbelt(&self, mode: PermissionMode) -> String {
        let mut s = String::new();
        s.push_str("(version 1)\n");
        s.push_str("(allow default)\n");
        s.push_str("(deny file-write*)\n");
        // The writable allow-list is non-empty only under `workspace-write`.
        // Filter out the filesystem root: admitting `/` would make the entire
        // disk writable, turning the sandbox into a no-op (a session launched
        // from `/` is confined to the temp areas only).
        if mode == PermissionMode::WorkspaceWrite {
            for root in writable_roots(&self.workspace_root)
                .into_iter()
                .filter(|r| r.parent().is_some())
            {
                s.push_str(&format!(
                    "(allow file-write* (subpath \"{}\"))\n",
                    escape_seatbelt_path(&root)
                ));
            }
        }
        for dev in ["/dev/null", "/dev/zero", "/dev/stdout", "/dev/stderr"] {
            s.push_str(&format!("(allow file-write* (literal \"{dev}\"))\n"));
        }
        s
    }

    /// The `sandbox-exec` argv for `command` (single argv element to
    /// `bash -c` — no re-evaluation, no injection). Cross-platform so the
    /// seatbelt renderer (and its tests) compile on every target even though
    /// only macOS actually runs `sandbox-exec`.
    #[cfg(target_os = "macos")]
    pub fn wrap_argv(&self, command: &str, mode: PermissionMode) -> Vec<String> {
        vec![
            "-p".to_string(),
            self.render_seatbelt(mode),
            "--".to_string(),
            "bash".to_string(),
            "-c".to_string(),
            command.to_string(),
        ]
    }

    /// Build the `sandbox-exec` invocation for a command. The login shell's
    /// PATH is injected so the sandboxed bash finds Homebrew / toolchain
    /// binaries the GUI process env otherwise lacks (thread `e5047fd2`:
    /// `gh` not found). Non-interactive editor/pager env is injected when
    /// unset so git does not open an interactive `$EDITOR`/pager and hang
    /// the turn.
    #[cfg(target_os = "macos")]
    pub fn wrap_command(
        &self,
        command: &str,
        cwd: &Path,
        mode: PermissionMode,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
        cmd.args(self.wrap_argv(command, mode))
            .env("PATH", login_shell_path())
            .current_dir(cwd);
        inject_noninteractive_env(&mut cmd);
        cmd
    }
}

/// Escape a path for a seatbelt `(subpath "...")` string literal.
fn escape_seatbelt_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Whether the OS sandbox backend is available on the current platform.
pub fn is_available() -> bool {
    cfg!(target_os = "macos") && std::path::Path::new("/usr/bin/sandbox-exec").exists()
}

/// Non-interactive editor/pager defaults (see retired-harness rationale).
pub const NONINTERACTIVE_ENV: &[(&str, &str)] = &[
    ("GIT_EDITOR", "true"),
    ("EDITOR", "true"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
];

/// Inject the non-interactive editor/pager defaults on every platform (the
/// sandboxed backend only runs on macOS, but the unsandboxed backend is
/// cross-platform).
fn inject_noninteractive_env(cmd: &mut tokio::process::Command) {
    for (k, v) in NONINTERACTIVE_ENV {
        if std::env::var_os(k).is_none() {
            cmd.env(k, v);
        }
    }
}

/// The login shell's PATH (cached), with a conservative fallback.
#[cfg(target_os = "macos")] // consumers: the seatbelt `wrap_command` + unsandboxed backend
fn login_shell_path() -> String {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let default = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
        let Ok(shell) = std::env::var("SHELL") else {
            return default.to_string();
        };
        if shell.is_empty() {
            return default.to_string();
        }
        match std::process::Command::new(&shell)
            .arg("-l")
            .arg("-c")
            .arg("printf %s \"$PATH\"")
            .output()
        {
            Ok(out) if out.status.success() => {
                let path = String::from_utf8_lossy(&out.stdout).to_string();
                if path.is_empty() {
                    default.to_string()
                } else {
                    path
                }
            }
            _ => default.to_string(),
        }
    })
    .clone()
}

/// One-shot seatbelt-wrapped bash backend for the pi bash tool. The effective
/// mode is resolved per call by the host-injected `mode_resolver` (the
/// session mode, or an approved `sandbox_permissions` grant for one call) —
/// the kernel `BashOperations` trait stays untouched, so the mode travels
/// through a closure, not a kernel field.
pub struct SandboxedBashOperations {
    policy: SandboxPolicy,
    base_cwd: PathBuf,
    mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
}

impl SandboxedBashOperations {
    pub fn new(
        base_cwd: impl Into<PathBuf>,
        policy: SandboxPolicy,
        mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync>,
    ) -> Self {
        Self {
            policy,
            base_cwd: base_cwd.into(),
            mode_resolver,
        }
    }

    /// Build the seatbelt-wrapped command for a background task — the same
    /// per-call mode and env as a foreground call, so a non-escalated
    /// background task is confined identically. The registry adds
    /// process-group/pipes after this. Non-macOS stub is unreachable: the
    /// wrap closure is only installed where `is_available()`.
    pub fn wrap_background(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        #[cfg(target_os = "macos")]
        {
            let mode = (self.mode_resolver)();
            self.policy.wrap_command(command, cwd, mode)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command).current_dir(cwd);
            c
        }
    }
}

#[async_trait::async_trait]
impl BashOperations for SandboxedBashOperations {
    async fn exec(&self, request: BashExecRequest<'_>) -> Result<CommandResult, ExecutionError> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = request;
            return Err(ExecutionError::Other(
                "seatbelt sandbox is only available on macOS".into(),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            let cwd = request
                .cwd
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.base_cwd.clone());
            let mode = (self.mode_resolver)();
            let mut cmd = self.policy.wrap_command(request.command, &cwd, mode);
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let child = cmd
                .spawn()
                .map_err(|e| ExecutionError::Spawn(e.to_string()))?;
            let timeout = request.timeout.unwrap_or(DEFAULT_TIMEOUT);
            run_to_completion(child, request.on_data, timeout, &request.signal).await
        }
    }
}

/// Drain a spawned child to completion with streaming output, wall-clock
/// timeout, and cancellation. Shared by the sandboxed and unsandboxed
/// backends: both build a `tokio::process::Command`, spawn, then converge
/// on the same semantics (drop kills the direct child via `kill_on_drop`,
/// even when the enclosing exec future is dropped from outside).
async fn run_to_completion(
    child: tokio::process::Child,
    on_data: Option<pi::tools::bash::BashDataCallback<'_>>,
    timeout: Duration,
    signal: &CancellationToken,
) -> Result<CommandResult, ExecutionError> {
    let mut child = child;
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    use tokio::io::AsyncReadExt as _;
    let drain = async {
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let mut out_chunk = [0u8; 8192];
        let mut err_chunk = [0u8; 8192];
        let mut out_done = false;
        let mut err_done = false;
        loop {
            if out_done && err_done {
                break;
            }
            tokio::select! {
                r = stdout.read(&mut out_chunk), if !out_done => match r {
                    Ok(0) => out_done = true,
                    Ok(n) => {
                        out_buf.extend_from_slice(&out_chunk[..n]);
                        if let Some(cb) = on_data { cb(&out_chunk[..n]); }
                    }
                    Err(_) => out_done = true,
                },
                r = stderr.read(&mut err_chunk), if !err_done => match r {
                    Ok(0) => err_done = true,
                    Ok(n) => {
                        err_buf.extend_from_slice(&err_chunk[..n]);
                        if let Some(cb) = on_data { cb(&err_chunk[..n]); }
                    }
                    Err(_) => err_done = true,
                },
            }
        }
        let status = child.wait().await;
        (out_buf, err_buf, status)
    };

    let (out_buf, err_buf, status) = tokio::select! {
        r = drain => r,
        _ = signal.cancelled() => {
            // Dropping the child (kill_on_drop) kills the direct child. A
            // compound command's grandchildren can outlive it; `bash -c`
            // execs the single-command common case, so the direct child IS
            // the command there.
            return Err(ExecutionError::Aborted);
        }
        _ = tokio::time::sleep(timeout) => {
            return Err(ExecutionError::Timeout(timeout));
        }
    };

    let status = status.map_err(|e| ExecutionError::Other(e.to_string()))?;
    Ok(CommandResult {
        stdout: String::from_utf8_lossy(&out_buf).into_owned(),
        stderr: String::from_utf8_lossy(&err_buf).into_owned(),
        exit_code: status.code().unwrap_or(-1),
    })
}

/// One-shot unsandboxed bash backend: direct `bash -c` execution with no
/// seatbelt confinement. Selected per call when the effective mode is
/// `danger-full-access`; the same non-interactive git env and login PATH are
/// injected so escalated git/gh runs behave identically minus the
/// confinement.
pub struct UnsandboxedBashOperations {
    base_cwd: PathBuf,
}

impl UnsandboxedBashOperations {
    pub fn new(base_cwd: impl Into<PathBuf>) -> Self {
        Self {
            base_cwd: base_cwd.into(),
        }
    }
}

#[async_trait::async_trait]
impl BashOperations for UnsandboxedBashOperations {
    async fn exec(&self, request: BashExecRequest<'_>) -> Result<CommandResult, ExecutionError> {
        let cwd = request
            .cwd
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.base_cwd.clone());
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(request.command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(target_os = "macos")]
        cmd.env("PATH", login_shell_path());
        inject_noninteractive_env(&mut cmd);
        let child = cmd
            .spawn()
            .map_err(|e| ExecutionError::Spawn(e.to_string()))?;
        let timeout = request.timeout.unwrap_or(DEFAULT_TIMEOUT);
        run_to_completion(child, request.on_data, timeout, &request.signal).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_root() -> PathBuf {
        canonicalize_best_effort(&std::env::temp_dir()).join("manox-sandbox-test-proj")
    }

    fn resolver(mode: PermissionMode) -> Arc<dyn Fn() -> PermissionMode + Send + Sync> {
        Arc::new(move || mode)
    }

    #[test]
    fn for_project_writable_roots_include_tmp_and_tmpdir() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let policy = SandboxPolicy::for_project(&root);
        let sb = policy.render_seatbelt(PermissionMode::WorkspaceWrite);
        assert!(sb.starts_with("(version 1)\n(allow default)\n(deny file-write*)\n"));
        // The workspace root is admitted.
        assert!(sb.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            root.display()
        )));
        // `/tmp` is admitted (canonicalized to /private/tmp on macOS).
        assert!(sb.contains("(allow file-write* (subpath \"/tmp\"))") || sb.contains("(allow file-write* (subpath \"/private/tmp\"))"));
    }

    #[test]
    fn read_only_profile_admits_no_writable_roots() {
        let root = project_root();
        let policy = SandboxPolicy::for_project(&root);
        let sb = policy.render_seatbelt(PermissionMode::ReadOnly);
        assert!(sb.contains("(deny file-write*)"));
        // No writable-root allow rules under read-only — only the dev sinks.
        assert!(!sb.contains("(allow file-write* (subpath"));
        assert!(sb.contains("(allow file-write* (literal \"/dev/null\"))"));
    }

    #[test]
    fn seatbelt_has_no_network_deny_or_git_protection() {
        let root = project_root();
        let policy = SandboxPolicy::for_project(&root);
        let sb = policy.render_seatbelt(PermissionMode::WorkspaceWrite);
        // Pure file-effect vocabulary: no network rule, no `.git` deny.
        assert!(!sb.contains("(deny network*)"));
        assert!(!sb.contains(".git"));
    }

    #[test]
    fn filesystem_root_is_filtered_out() {
        // Launching from `/` must not admit the entire disk: the fs root is
        // filtered from the writable allow-list.
        let policy = SandboxPolicy::for_project(Path::new("/"));
        let sb = policy.render_seatbelt(PermissionMode::WorkspaceWrite);
        assert!(
            !sb.contains("(allow file-write* (subpath \"/\"))"),
            "fs root not admitted: {sb}"
        );
    }

    #[test]
    fn worktree_policy_scopes_to_worktree() {
        let wt = project_root().join("wt");
        let policy = SandboxPolicy::for_worktree(&wt);
        let sb = policy.render_seatbelt(PermissionMode::WorkspaceWrite);
        assert!(sb.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            canonicalize_best_effort(&wt).display()
        )));
        assert_eq!(
            policy.worktree_anchor(),
            Some(canonicalize_best_effort(&wt).as_path())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_argv_shapes_the_invocation() {
        let policy = SandboxPolicy::for_project(&project_root());
        let args = policy.wrap_argv("echo hi", PermissionMode::WorkspaceWrite);
        assert_eq!(args[0], "-p");
        assert!(args[1].contains("(deny file-write*)"));
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "bash");
        assert_eq!(args[4], "-c");
        assert_eq!(args[5], "echo hi", "command is a single argv element");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandboxed_exec_runs_and_confines_writes() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let policy = SandboxPolicy::for_project(&root);
        let ops = SandboxedBashOperations::new(&root, policy, resolver(PermissionMode::WorkspaceWrite));

        // A confined write inside the project succeeds.
        let inside = root.join("sandbox-ok.txt");
        let _ = std::fs::remove_file(&inside);
        let req = BashExecRequest {
            command: &format!("echo hi > {}", inside.display()),
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let res = ops.exec(req).await.expect("confined write runs");
        if res.exit_code == 71 && res.stderr.contains("Operation not permitted") {
            // Nested sandbox-exec is refused when the PARENT process is
            // itself sandboxed. Environment-specific — CI and the GUI app
            // are unsandboxed and exercise the real confinement.
            eprintln!("skipping seatbelt assertions: parent process sandboxed");
            return;
        }
        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert!(inside.exists(), "file written inside project");
        let _ = std::fs::remove_file(&inside);

        // A write outside the project + temp areas is denied by seatbelt.
        let denied_target = PathBuf::from("/usr/local/manox-sandbox-deny-target");
        let req = BashExecRequest {
            command: &format!("touch {}", denied_target.display()),
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let res = ops.exec(req).await.expect("exec itself succeeds");
        assert_ne!(res.exit_code, 0, "seatbelt denies the write");
        assert!(!denied_target.exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn read_only_seatbelt_denies_writes_but_allows_reads() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let inside = root.join("ro-target.txt");
        let _ = std::fs::remove_file(&inside);
        let policy = SandboxPolicy::for_project(&root);
        let ops = SandboxedBashOperations::new(&root, policy, resolver(PermissionMode::ReadOnly));

        // A write inside the project is denied under read-only.
        let req = BashExecRequest {
            command: &format!("echo hi > {}", inside.display()),
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let res = ops.exec(req).await.expect("exec runs");
        if res.exit_code == 71 && res.stderr.contains("Operation not permitted") {
            eprintln!("skipping seatbelt assertions: parent process sandboxed");
            return;
        }
        assert_ne!(res.exit_code, 0, "read-only denies the write");
        assert!(!inside.exists());

        // A read succeeds (reads are unrestricted in every mode).
        let req = BashExecRequest {
            command: "ls /dev/null",
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let res = ops.exec(req).await.expect("read runs");
        assert_eq!(res.exit_code, 0, "read succeeds under read-only: {}", res.stderr);
    }

    #[tokio::test]
    async fn unsandboxed_exec_runs_without_confinement() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let ops = UnsandboxedBashOperations::new(&root);

        let req = BashExecRequest {
            command: "printf 'hello unsandboxed'",
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let res = ops.exec(req).await.expect("unsandboxed exec runs");
        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert_eq!(res.stdout, "hello unsandboxed");
    }

    #[tokio::test]
    async fn unsandboxed_exec_respects_timeout() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let ops = UnsandboxedBashOperations::new(&root);

        let req = BashExecRequest {
            command: "sleep 30",
            cwd: Some(&root),
            timeout: Some(Duration::from_millis(100)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let err = ops.exec(req).await.expect_err("sleep outlives the timeout");
        assert!(matches!(err, ExecutionError::Timeout(_)), "{err:?}");
    }

    #[tokio::test]
    async fn unsandboxed_exec_respects_cancellation() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let ops = UnsandboxedBashOperations::new(&root);
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();

        let req = BashExecRequest {
            command: "sleep 30",
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal,
            on_data: None,
        };
        let err = ops.exec(req).await.expect_err("pre-cancelled token aborts");
        assert!(matches!(err, ExecutionError::Aborted), "{err:?}");
    }
}
