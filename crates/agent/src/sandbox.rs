//! OS-level sandbox for the pi-path `bash` tool: macOS seatbelt
//! (`sandbox-exec`) wrapping one-shot commands (ported from the retired
//! manox harness).
//!
//! ## Threat model
//!
//! Thread `c5aefe4d` escaped the prior brush-only bash: `cd` into a sibling
//! worktree and `git commit`/`rebase`/`push` against its `.git` — no
//! confinement. The sandbox blocks exactly that class: writes outside the
//! project root + temp dir, writes to `.git`, and all network. Reads and
//! process execution stay unrestricted (the model legitimately reads system
//! files and runs binaries); the sandbox confines writes + network but not
//! reads.
//!
//! ## Backend
//!
//! macOS: [`SandboxPolicy::wrap_command`] wraps the command in
//! `sandbox-exec -p POLICY -- bash -c "<command>"`. The command is a single
//! argv element — zero shell escaping, no injection surface — and seatbelt's
//! process-level inheritance covers bash and every descendant.
//!
//! The pi bash tool's default backend is the brush-based persistent shell;
//! when the seatbelt backend is available the engine installs
//! [`SandboxedBashOperations`] instead — one-shot seatbelt-wrapped
//! executions, matching the retired harness's default sandboxed path (the
//! persistent shell was its unsandboxed escape hatch there too). Shell state
//! (`cd`/exports) does not persist across sandboxed calls; the tool's `cwd`
//! parameter pins the working directory per call.
//!
//! Escalation: the engine also installs [`UnsandboxedBashOperations`] into
//! the bash tool's escalation slot — direct `bash -c` with no confinement.
//! A call escalates when the model passes `unsandboxed: true` or the host's
//! force resolver (Danger mode) says so; authorization is host policy, never
//! the model's word.
//!
//! ## Honest gaps
//!
//! - Linux/Windows: [`is_available`] returns false; the engine falls back to
//!   the unsandboxed persistent shell (approval-gated as always). FS write
//!   confinement for the file tools is covered separately by
//!   `crate::path_policy`.
//! - Network policy: `Blocked` denies all; `Restricted` (non-empty
//!   `[network] allowlist`) narrows outbound to the in-process allowlist
//!   proxy; `Unrestricted` (worktree sessions) admits all.
//! - Background bash tasks (`run_in_background`) are seatbelt-wrapped via
//!   the registry's sandbox wrapper with the same policy; escalated
//!   backgrounds stay bare.
//! - The seatbelt policy is a denylist over non-write syscalls (`(allow
//!   default)` base) and an allowlist over writes (`deny file-write*` +
//!   narrow `allow` for writable roots + `deny` for protected paths). A
//!   stricter `(deny default)` allowlist is future work.

use std::path::{Path, PathBuf};

use pi::env::{CommandResult, ExecutionError};
use pi::tools::bash::{BashExecRequest, BashOperations};
use tokio_util::sync::CancellationToken;
// Shared by the sandboxed and unsandboxed exec paths (both honor a
// wall-clock timeout; the seatbelt path was the original consumer).
use std::time::Duration;

/// Default wall-clock limit for a one-shot command (mirrors the bash tool's
/// own default).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Network policy for sandboxed bash.
#[derive(Clone, Debug, Default)]
pub enum NetworkPolicy {
    /// No network at all (`(deny network*)`).
    #[default]
    Blocked,
    /// No network rule: the `(allow default)` base admits all network (the
    /// worktree relaxation).
    Unrestricted,
    /// Hostname allowlist mode: the seatbelt narrows outbound to a local
    /// in-process HTTP proxy port, and the proxy (`crates/http_proxy`)
    /// enforces the hostname patterns (exact + `*.subdomain` wildcards).
    Restricted { allowlist: Vec<String> },
}

impl NetworkPolicy {
    /// Policy from settings: an empty `[network] allowlist` yields
    /// `Blocked`; a non-empty one yields `Restricted` (patterns retained).
    pub fn from_settings() -> NetworkPolicy {
        if cfg!(test) {
            // Hermetic tests never read the developer's real settings.
            return NetworkPolicy::Blocked;
        }
        let allowlist = crate::settings::load().network.allowlist.clone();
        if allowlist.is_empty() {
            NetworkPolicy::Blocked
        } else {
            NetworkPolicy::Restricted { allowlist }
        }
    }

    pub fn is_restricted(&self) -> bool {
        matches!(self, NetworkPolicy::Restricted { .. })
    }
}

/// Confinement policy for one sandboxed invocation. Derived from the project
/// root; the writable set is the project root plus the system temp dir, the
/// protected set is the project's `.git`.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    /// FS-side writable roots shared with the seatbelt renderer.
    writable_roots: Vec<PathBuf>,
    protected_paths: Vec<PathBuf>,
    network: NetworkPolicy,
    /// Subtrees of `protected_paths` explicitly re-opened for writes — the
    /// bound repo's shared `.git`, while a worktree is active.
    git_allowed_roots: Vec<PathBuf>,
    /// The active worktree root when this policy is worktree-scoped.
    worktree_anchor: Option<PathBuf>,
    /// Whether `/tmp` + `/private/tmp` are admitted as scratch space for the
    /// FS write check only — `true` in project mode, `false` under a
    /// worktree. Never admitted to the seatbelt: a sandboxed bash must not
    /// reach a sibling repo's `.git` under `/tmp` (the c5aefe4d escape).
    admit_tmp_scratch: bool,
}

fn temp_root() -> PathBuf {
    canonicalize_best_effort(&std::env::temp_dir())
}

/// Whether `canon` falls under `/tmp` or `/private/tmp` (the conventional
/// scratch locations distinct from `$TMPDIR`).
fn is_under_tmp(canon: &Path) -> bool {
    [Path::new("/tmp"), Path::new("/private/tmp")]
        .iter()
        .any(|t| canon.starts_with(t))
}

impl SandboxPolicy {
    /// Build the default policy for `project_root`. Roots are canonicalized
    /// best-effort so the Rust-side path checks (and seatbelt `subpath`
    /// matching, which resolves symlinks) compare against real paths — the
    /// temp dir is a symlink to `/private/var/...` on macOS.
    ///
    /// When `project_root` is the filesystem root (`/`), the writable set is
    /// narrowed to the temp dir only: admitting `/` would make the entire
    /// disk writable, turning the sandbox into a no-op (thread 6cd3d096).
    pub fn for_project(project_root: &Path) -> Self {
        let root = canonicalize_best_effort(project_root);
        if root.parent().is_none() {
            tracing::warn!(
                root = %root.display(),
                "sandbox project root is the filesystem root; narrowing writable set to temp dir only — launch manox from a real project directory to restore full confinement"
            );
            return Self {
                writable_roots: vec![temp_root()],
                protected_paths: Vec::new(),
                network: NetworkPolicy::from_settings(),
                git_allowed_roots: Vec::new(),
                worktree_anchor: None,
                admit_tmp_scratch: true,
            };
        }
        Self {
            writable_roots: vec![root.clone(), temp_root()],
            protected_paths: vec![root.join(".git")],
            network: NetworkPolicy::from_settings(),
            git_allowed_roots: Vec::new(),
            worktree_anchor: None,
            admit_tmp_scratch: true,
        }
    }

    /// Extend a project policy for an active worktree: confine writes to the
    /// worktree (+ temp), re-open the bound repo's shared `.git` for writes,
    /// and enable network — a worktree is an approved isolation context.
    /// The worktree's own `.git` pointer file stays protected: overwriting
    /// it redirects git to an attacker-controlled dir (the c5aefe4d escape
    /// class), so it is denied even though the worktree root is writable.
    pub fn with_worktree(mut self, worktree_path: &Path, main_repo_git_dir: &Path) -> Self {
        let worktree = canonicalize_best_effort(worktree_path);
        self.writable_roots = vec![worktree.clone(), temp_root()];
        self.protected_paths.push(worktree.join(".git"));
        self.git_allowed_roots
            .push(canonicalize_best_effort(main_repo_git_dir));
        self.network = NetworkPolicy::Unrestricted;
        self.worktree_anchor = Some(worktree);
        self.admit_tmp_scratch = false;
        self
    }

    /// Policy for a worktree-isolated sub-agent: write only its own worktree
    /// (+ temp), git ops against the bound repo's shared `.git`, network on.
    /// The worktree's own `.git` pointer file stays protected (see
    /// [`Self::with_worktree`]).
    pub fn for_worktree(worktree_path: &Path, main_repo_git_dir: &Path) -> Self {
        let worktree = canonicalize_best_effort(worktree_path);
        Self {
            writable_roots: vec![worktree.clone(), temp_root()],
            protected_paths: vec![worktree.join(".git")],
            network: NetworkPolicy::Unrestricted,
            git_allowed_roots: vec![canonicalize_best_effort(main_repo_git_dir)],
            worktree_anchor: Some(worktree),
            admit_tmp_scratch: false,
        }
    }

    /// The active worktree root when this policy is worktree-scoped.
    pub fn worktree_anchor(&self) -> Option<&Path> {
        self.worktree_anchor.as_deref()
    }

    pub fn network(&self) -> &NetworkPolicy {
        &self.network
    }

    /// Whether `path` falls under a writable root (best-effort
    /// canonicalization for not-yet-existing paths).
    pub fn is_writable(&self, path: &Path) -> bool {
        let canon = canonicalize_best_effort(path);
        if self
            .writable_roots
            .iter()
            .any(|root| canon.starts_with(root))
        {
            return true;
        }
        self.admit_tmp_scratch && is_under_tmp(&canon)
    }

    /// Whether `path` is protected: any `.git` path component, plus the
    /// explicit `protected_paths` set — minus `git_allowed_roots` re-opens.
    pub fn is_protected(&self, path: &Path) -> bool {
        let canon = canonicalize_best_effort(path);
        let has_git_component = canon
            .components()
            .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"));
        let under_protected =
            has_git_component || self.protected_paths.iter().any(|p| canon.starts_with(p));
        if !under_protected {
            return false;
        }
        let under_git_allowed = self.git_allowed_roots.iter().any(|g| canon.starts_with(g));
        !under_git_allowed
    }

    /// The combined write decision: writable AND not protected.
    pub fn is_write_allowed(&self, path: &Path) -> bool {
        self.is_writable(path) && !self.is_protected(path)
    }

    /// Render a seatbelt (`.sbpl`) policy string. Denylist base (`(allow
    /// default)`) with an allowlist over writes; network per policy.
    ///
    /// `proxy_port` is required when `network` is `Restricted`: the rule
    /// denies all network then re-allows outbound to `localhost:<port>`
    /// only, so the sandboxed process can reach the in-process allowlist
    /// proxy and nothing else.
    pub fn render_seatbelt(&self, proxy_port: Option<u16>) -> String {
        let mut s = String::new();
        s.push_str("(version 1)\n");
        s.push_str("(allow default)\n");
        s.push_str("(deny file-write*)\n");
        for root in &self.writable_roots {
            s.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                escape_seatbelt_path(root)
            ));
        }
        for dev in ["/dev/null", "/dev/zero", "/dev/stdout", "/dev/stderr"] {
            s.push_str(&format!("(allow file-write* (literal \"{dev}\"))\n"));
        }
        for p in &self.protected_paths {
            s.push_str(&format!(
                "(deny file-write* (subpath \"{}\"))\n",
                escape_seatbelt_path(p)
            ));
        }
        // Re-allow the bound repo's `.git` AFTER the protected denies so a
        // linked worktree's git ops succeed (seatbelt last-match-wins).
        for g in &self.git_allowed_roots {
            s.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                escape_seatbelt_path(g)
            ));
        }
        match &self.network {
            NetworkPolicy::Blocked => {
                s.push_str("(deny network*)\n");
            }
            NetworkPolicy::Unrestricted => {
                // No network rule: the `(allow default)` base admits all.
            }
            NetworkPolicy::Restricted { .. } => {
                // Deny all network, then re-allow outbound to the local
                // proxy port only. The proxy (running outside the sandbox)
                // enforces the hostname allowlist. Seatbelt network
                // filters accept only `*` or `localhost` as the host, so
                // the rule names `localhost`; it matches connections to
                // 127.0.0.1 (an IP literal is rejected at compile time).
                // `network-bind` is re-allowed for localhost so TCP
                // connect() (which implicitly binds an ephemeral port)
                // works inside the sandbox.
                let port = proxy_port.unwrap_or(0);
                s.push_str("(deny network*)\n");
                s.push_str(&format!(
                    "(allow network-outbound (remote tcp \"localhost:{port}\"))\n"
                ));
                s.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
            }
        }
        s
    }

    /// Wrap a bash command in a `sandbox-exec` invocation. `command` is
    /// passed as a single argv element to `bash -c`, so the model's command
    /// string is never re-evaluated by an outer shell — no escaping, no
    /// injection. The login shell's PATH is injected so the sandboxed bash
    /// finds Homebrew / toolchain binaries the GUI process env otherwise
    /// The `sandbox-exec` argv for `command` (single argv element to
    /// `bash -c` — no re-evaluation, no injection). Cross-platform so the
    /// seatbelt renderer (and its tests) compile on every target even though
    /// only macOS actually runs `sandbox-exec`.
    #[cfg(target_os = "macos")]
    pub fn wrap_argv(&self, command: &str, proxy_port: Option<u16>) -> Vec<String> {
        vec![
            "-p".to_string(),
            self.render_seatbelt(proxy_port),
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
    ///
    /// `proxy_port` must be `Some(port)` when `network` is `Restricted`:
    /// the seatbelt rule narrows outbound to `localhost:<port>` and the
    /// proxy env vars (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` + lowercase)
    /// are injected so the sandboxed command's HTTP clients route through
    /// the in-process allowlist proxy. `NO_PROXY` is blanked so all egress
    /// goes through the proxy unconditionally.
    #[cfg(target_os = "macos")]
    pub fn wrap_command(
        &self,
        command: &str,
        cwd: &Path,
        proxy_port: Option<u16>,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
        cmd.args(self.wrap_argv(command, proxy_port))
            .env("PATH", login_shell_path())
            .current_dir(cwd);
        inject_noninteractive_env(&mut cmd);
        if let NetworkPolicy::Restricted { .. } = &self.network
            && let Some(port) = proxy_port
        {
            let proxy_url = format!("http://127.0.0.1:{port}");
            for key in [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
            ] {
                cmd.env(key, &proxy_url);
            }
            for key in ["NO_PROXY", "no_proxy"] {
                cmd.env(key, "");
            }
        }
        cmd
    }
}

/// Canonicalize a path that may not yet exist: resolve the longest existing
/// ancestor and rejoin the remaining tail. Falls back to the raw path when
/// no ancestor exists.
pub fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if path.exists() {
        return path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    if parent == Path::new("") {
        return path.to_path_buf();
    }
    let canon_parent = canonicalize_best_effort(parent);
    match path.file_name() {
        Some(name) => canon_parent.join(name),
        None => canon_parent,
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

/// The login shell's PATH (cached), with a conservative fallback — mirrors
/// `path_env` (PR #467); consolidated after both land.
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

/// One-shot seatbelt-wrapped bash backend for the pi bash tool (see module
/// docs for the semantics vs the persistent brush backend).
pub struct SandboxedBashOperations {
    policy: SandboxPolicy,
    // Read only by the macOS exec branch; on other platforms the exec stub
    // returns before touching it, so suppress the dead-code lint there.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    base_cwd: PathBuf,
    // Lazily-spawned allowlist proxy for `Restricted` network policies.
    // Held here (not in the policy) because `SandboxPolicy` is a plain
    // value shared by value; the handle must outlive every exec it serves.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    proxy: std::sync::Mutex<Option<http_proxy::ProxyHandle>>,
}

impl SandboxedBashOperations {
    pub fn new(base_cwd: impl Into<PathBuf>, policy: SandboxPolicy) -> Self {
        Self {
            policy,
            base_cwd: base_cwd.into(),
            proxy: std::sync::Mutex::new(None),
        }
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// Build the seatbelt-wrapped command for a background task — the same
    /// policy, allowlist proxy, and env as a foreground call, so a
    /// non-escalated background task is confined identically. The registry
    /// adds process-group/pipes after this. Non-macOS stub is unreachable:
    /// the wrap closure is only installed where `is_available()`.
    pub fn wrap_background(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        #[cfg(target_os = "macos")]
        {
            // A proxy failure here cannot surface through the Command
            // builder; degrade to no-proxy (the command's network calls
            // fail, visibly) rather than panic. Foreground calls propagate
            // the error properly via `exec`.
            let proxy_port = self.ensure_proxy().ok().flatten();
            self.policy.wrap_command(command, cwd, proxy_port)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(command).current_dir(cwd);
            c
        }
    }

    /// The allowlist-proxy port for a `Restricted` network policy, spawning
    /// the proxy on first use. `Ok(None)` for `Blocked`/`Unrestricted`. The
    /// proxy is pinned per backend instance so its port stays stable across
    /// the calls it serves (each session rebuild spawns its own). A spawn
    /// failure (or an allowlist with no valid entries) is an `Err` — the
    /// caller surfaces it instead of silently degrading `Restricted` to a
    /// half-broken deny.
    // macOS-only like its callers (seatbelt wrap paths + their tests); other
    // platforms compile it out so `-D warnings` stays clean under clippy.
    #[cfg(target_os = "macos")]
    fn ensure_proxy(&self) -> Result<Option<u16>, ExecutionError> {
        let NetworkPolicy::Restricted { allowlist } = self.policy.network() else {
            return Ok(None);
        };
        let mut guard = self.proxy.lock().expect("proxy lock poisoned");
        if let Some(handle) = guard.as_ref() {
            return Ok(Some(handle.port()));
        }
        // Parse the settings patterns into the proxy's allowlist; malformed
        // entries are skipped (a typo must not block the whole list).
        let patterns: Vec<http_proxy::HostPattern> = allowlist
            .iter()
            .filter_map(|raw| match http_proxy::HostPattern::parse(raw) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(raw = raw.as_str(), error = %e, "skipping malformed network allowlist entry");
                    None
                }
            })
            .collect();
        if patterns.is_empty() {
            return Err(ExecutionError::Other(format!(
                "sandbox network proxy unavailable: no valid hostname patterns in `[network] allowlist` ({}). \
                 Fix the allowlist in settings, or use `unsandboxed: true` (user approval).",
                allowlist.join(", ")
            )));
        }
        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        let config = http_proxy::ProxyConfig {
            allowlist: http_proxy::Allowlist::from_patterns(patterns),
            events: events_tx,
        };
        let handle = http_proxy::ProxyHandle::spawn(config).map_err(|e| {
            ExecutionError::Other(format!(
                "sandbox network proxy unavailable: {e}. \
                 Use `unsandboxed: true` (user approval) or fix the allowlist settings."
            ))
        })?;
        let port = handle.port();
        // Drain events so the unbounded channel never fills and blocks
        // connection threads; events are diagnostics only.
        tokio::spawn(async move {
            use futures::StreamExt as _;
            while let Some(event) = events_rx.next().await {
                tracing::debug!(?event, "sandbox network proxy");
            }
        });
        tracing::debug!(port, "sandbox network proxy ready");
        *guard = Some(handle);
        Ok(Some(port))
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
            let proxy_port = self.ensure_proxy()?;
            let mut cmd = self.policy.wrap_command(request.command, &cwd, proxy_port);
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
/// on the same semantics (drop kills the tree via `kill_on_drop`).
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
            // Dropping the child (kill_on_drop) kills the process tree.
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
/// seatbelt confinement. Installed as the `BashTool` escalation slot and
/// selected by Danger mode (host-forced) or a per-call `unsandboxed: true`;
/// the escalation counterpart of [`SandboxedBashOperations`]. The same
/// non-interactive git env and login PATH are injected so escalated git/gh
/// runs behave identically minus the confinement.
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

    #[test]
    fn for_project_confines_writes_and_protects_git() {
        let root = project_root();
        std::fs::create_dir_all(root.join(".git")).ok();
        let policy = SandboxPolicy::for_project(&root);

        assert!(policy.is_write_allowed(&root.join("src/main.rs")));
        assert!(policy.is_write_allowed(&root.join("new-dir/file.txt")));
        assert!(
            !policy.is_write_allowed(&root.join(".git/config")),
            ".git protected"
        );
        // A sibling repo OUTSIDE the writable roots is confined ($TMPDIR is
        // itself a writable root, so pick a system location).
        let sibling = PathBuf::from("/usr/local/manox-sandbox-sibling-proj");
        assert!(
            !policy.is_write_allowed(&sibling.join("file")),
            "sibling confined"
        );
        // Any `.git` component stays protected wherever it lives (the
        // c5aefe4d escape class), including under the admitted temp dir.
        let tmp_sibling_git =
            canonicalize_best_effort(&std::env::temp_dir()).join("other-proj/.git/config");
        assert!(
            policy.is_protected(&tmp_sibling_git),
            "sibling .git protected"
        );
        // /tmp scratch admitted for FS writes in project mode.
        assert!(policy.is_writable(Path::new("/tmp/manox-scratch")));
    }

    #[test]
    fn filesystem_root_degenerates_to_temp_only() {
        let policy = SandboxPolicy::for_project(Path::new("/"));
        assert!(!policy.is_write_allowed(Path::new("/etc/passwd")));
        assert!(policy.is_writable(Path::new("/tmp/ok")));
    }

    #[test]
    fn worktree_policy_reopens_bound_git_and_denies_tmp() {
        let wt = project_root().join("wt");
        let git_dir = project_root().join(".git");
        std::fs::create_dir_all(&git_dir).ok();
        let policy = SandboxPolicy::for_project(&project_root()).with_worktree(&wt, &git_dir);
        assert!(policy.is_write_allowed(&wt.join("file")));
        assert!(
            policy.is_write_allowed(&git_dir.join("refs/head")),
            "bound repo .git re-opened"
        );
        // The worktree's own `.git` pointer file stays protected even
        // though the worktree root is writable (gitdir redirect is the
        // c5aefe4d escape class).
        assert!(
            !policy.is_write_allowed(&wt.join(".git")),
            "worktree .git pointer protected"
        );
        let sibling_git = canonicalize_best_effort(&std::env::temp_dir()).join("other-proj/.git");
        assert!(
            policy.is_protected(&sibling_git.join("config")),
            "sibling .git stays protected"
        );
        // A path outside the temp dir entirely is not writable. (Cannot use
        // `/tmp/x` here: on Linux `temp_dir` IS `/tmp`, hence `temp_root`,
        // so `/tmp` is legitimately in the writable set; on macOS `temp_dir`
        // is `/var/folders/…`, distinct from `/tmp`.)
        assert!(
            !policy.is_writable(Path::new("/manox-sandbox-outside-temp-test/x")),
            "tmp not admitted under worktree"
        );
        assert_eq!(
            policy.worktree_anchor(),
            Some(canonicalize_best_effort(&wt).as_path())
        );
    }

    #[test]
    fn seatbelt_renders_write_allowlist_and_network_deny() {
        let root = project_root();
        std::fs::create_dir_all(root.join(".git")).ok();
        let policy = SandboxPolicy::for_project(&root);
        let sb = policy.render_seatbelt(None);
        assert!(sb.starts_with("(version 1)\n(allow default)\n(deny file-write*)\n"));
        assert!(sb.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            root.display()
        )));
        assert!(sb.contains(&format!(
            "(deny file-write* (subpath \"{}\"))",
            root.join(".git").display()
        )));
        assert!(sb.contains("(deny network*)"), "blocked network denies all");
    }

    #[test]
    fn seatbelt_restricted_renders_proxy_only_outbound() {
        let root = project_root();
        std::fs::create_dir_all(root.join(".git")).ok();
        let mut policy = SandboxPolicy::for_project(&root);
        policy.network = NetworkPolicy::Restricted {
            allowlist: vec!["github.com".into()],
        };
        let sb = policy.render_seatbelt(Some(43210));
        assert!(sb.contains("(deny network*)\n"));
        assert!(
            sb.contains("(allow network-outbound (remote tcp \"localhost:43210\"))"),
            "outbound narrowed to the local proxy port"
        );
        assert!(
            !sb.contains("127.0.0.1"),
            "seatbelt rejects IP literals; the localhost rule covers loopback"
        );
        assert!(sb.contains("(allow network-bind (local ip \"localhost:*\"))"));
        // The allowlist itself is NOT in the seatbelt — the proxy enforces it.
        assert!(!sb.contains("github.com"));
    }

    #[test]
    fn seatbelt_worktree_allows_git_after_deny() {
        let wt = project_root().join("wt2");
        let git_dir = project_root().join(".git");
        let policy = SandboxPolicy::for_project(&project_root()).with_worktree(&wt, &git_dir);
        let sb = policy.render_seatbelt(None);
        let deny_pos = sb.find("(deny network*)").unwrap_or(usize::MAX);
        let allow_git = sb
            .rfind(&format!(
                "(allow file-write* (subpath \"{}\"))",
                canonicalize_best_effort(&git_dir).display()
            ))
            .expect("git re-allow present");
        assert!(allow_git < deny_pos, "re-allow rendered in write section");
        assert!(
            !sb.contains("(deny network*)"),
            "worktree network unrestricted"
        );
        // The worktree `.git` pointer deny is emitted after the writable
        // allow, so seatbelt's last-match-wins blocks the redirect write.
        let wt_deny = sb.find(&format!(
            "(deny file-write* (subpath \"{}\"))",
            canonicalize_best_effort(&wt).join(".git").display()
        ));
        assert!(wt_deny.is_some(), "worktree .git pointer denied");
        let wt_allow = sb.find(&format!(
            "(allow file-write* (subpath \"{}\"))",
            canonicalize_best_effort(&wt).display()
        ));
        assert!(wt_allow.is_some() && wt_deny.unwrap() > wt_allow.unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_argv_shapes_the_invocation() {
        let policy = SandboxPolicy::for_project(&project_root());
        let args = policy.wrap_argv("echo hi", None);
        assert_eq!(args[0], "-p");
        assert!(args[1].contains("(deny file-write*)"));
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "bash");
        assert_eq!(args[4], "-c");
        assert_eq!(args[5], "echo hi", "command is a single argv element");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn restricted_policy_spawns_allowlist_proxy() {
        let root = project_root();
        std::fs::create_dir_all(root.join(".git")).ok();
        let mut policy = SandboxPolicy::for_project(&root);
        policy.network = NetworkPolicy::Restricted {
            allowlist: vec!["github.com".into()],
        };
        let ops = SandboxedBashOperations::new(&root, policy);
        let port = ops
            .ensure_proxy()
            .expect("proxy spawns")
            .expect("restricted port");
        assert!(port > 0, "proxy binds an ephemeral port");
        // The proxy is pinned per backend: repeat calls reuse the port.
        assert_eq!(ops.ensure_proxy().unwrap(), Some(port));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandboxed_exec_runs_and_confines_writes() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let policy = SandboxPolicy::for_project(&root);
        let ops = SandboxedBashOperations::new(&root, policy);

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
            // itself sandboxed (e.g. this test running under a CLI
            // sandbox). Environment-specific — CI and the GUI app are
            // unsandboxed and exercise the real confinement.
            eprintln!("skipping seatbelt assertions: parent process sandboxed");
            return;
        }
        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert!(inside.exists(), "file written inside project");
        let _ = std::fs::remove_file(&inside);

        // A write outside the project is denied by seatbelt (aim at a
        // non-temp location: the temp root is inside the writable set).
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
