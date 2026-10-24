//! OS-level sandbox for the kernel-path `bash` tool: macOS seatbelt
//! (`sandbox-exec`) wrapping one-shot commands, Linux bubblewrap + seccomp
//! (`sandbox::linux`) under the same writable-root vocabulary.
//!
//! The seatbelt renders a per-call profile from the effective `AccessMode`:
//! `sandbox` confines writes to the shared `writable_roots`; `full-access`
//! skips the seatbelt entirely (the unsandboxed backend, selected before any
//! profile is rendered).
//!
//! The base is deny-default and the profile curates its allows: full-disk
//! reads, per-root writes injected as `sandbox-exec -D` definitions (paths
//! travel as argv and are referenced through `(param ...)`, never embedded in
//! the profile text), process spawn, and the sysctl/pty minimums toolchains
//! probe for. The network sections follow the session's `networkAccess`
//! switch (the [`NetworkRoute`]): off renders no network allowance at all
//! (unix-socket IPC stays so subprocess plumbing survives), on-without-
//! allowlist keeps the historical full open with the platform's DNS/TLS
//! minimums, and the managed route admits only the loopback proxy. Writable
//! roots are validated at construction: a symlink inside a root's component
//! chain fails the build (fail-closed) while the platform's top-level aliases
//! (`/tmp` → `/private/tmp`) are normalized instead.
//!
//! The kernel `BashOperations` trait stays untouched (layering red line: no
//! domain field in the kernel). The per-call effective mode reaches the
//! renderer via a host-injected `mode_resolver` closure, mirroring the
//! existing `force_unsandboxed` injection pattern.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use manox_harness::env::{CommandResult, ExecutionError};
use manox_harness::sandbox::{AccessMode, NetworkAccess, writable_roots};
use manox_harness::tools::bash::{BashExecRequest, BashOperations};
use tokio_util::sync::CancellationToken;

// The one canonicalization home: the seatbelt and the fs fence share
// `manox_harness::sandbox`'s `..`-folding implementation so a traversal
// cannot survive into a classified path. Re-exported here for the host
// callers that still reach it as `crate::sandbox::canonicalize_best_effort`.
pub use manox_harness::sandbox::canonicalize_best_effort;

/// The Linux backend (bubblewrap + seccomp); compiled only on its target.
#[cfg(target_os = "linux")]
mod linux;

/// The managed-network half: the domain allowlist, the loopback CONNECT
/// proxy, and the route the profile renders. macOS-only in effect — the
/// Linux managed path needs a netns loopback bridge (a later stage).
pub mod netpolicy;

/// The network face one confined call runs under: the session's
/// `networkAccess` switch plus, when the managed proxy is running (macOS,
/// allowlist configured), the loopback port it listens on. `full-access`
/// calls never reach a renderer, so there is no "open" face here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkRoute {
    /// `networkAccess=off`: no network allowance is rendered; Linux unshares
    /// the network namespace and installs the socket-restricted seccomp face.
    Disabled,
    /// `networkAccess=on` with no allowlist: fully open, the pre-closure
    /// behavior byte for byte.
    Direct,
    /// `networkAccess=on` with an allowlist (macOS): the profile admits only
    /// the loopback proxy and loopback itself; non-proxied clients fail
    /// closed at the OS layer.
    Managed { proxy_port: u16 },
}

impl NetworkRoute {
    /// The face a bare access switch produces (no managed proxy).
    pub fn from_access(access: NetworkAccess) -> Self {
        match access {
            NetworkAccess::Off => Self::Disabled,
            NetworkAccess::On => Self::Direct,
        }
    }

    /// The access switch the route carries (the classifier reads this).
    pub fn access(self) -> NetworkAccess {
        match self {
            Self::Disabled => NetworkAccess::Off,
            Self::Direct | Self::Managed { .. } => NetworkAccess::On,
        }
    }
}

/// Default wall-clock limit for a one-shot command (mirrors the bash tool's
/// own default).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The deny-default base every profile shares: process minimums (children
/// inherit the policy they run under), unrestricted reads (this phase has no
/// deny-read vocabulary), the `/dev/null` sink, and the sysctl/mach/pty
/// minimums toolchains and runtimes probe for at startup. Curated after the
/// Chrome/codex seatbelt base policies — everything not listed here is
/// refused.
const SEATBELT_BASE_POLICY: &str = r#"(version 1)
(deny default)

; Child processes inherit this policy; job control and ps-style queries stay
; answerable within the same sandbox.
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))

; Reads are unrestricted in this phase (no deny-read vocabulary yet).
(allow file-read*)

; The null device absorbs discarded output.
(allow file-write-data
  (require-all
    (path "/dev/null")
    (vnode-type CHARACTER-DEVICE)))

; CPU/kernel probes toolchains and runtimes (rustc, node, python) make at
; startup.
(allow sysctl-read
  (sysctl-name "hw.activecpu")
  (sysctl-name "hw.busfrequency_compat")
  (sysctl-name "hw.byteorder")
  (sysctl-name "hw.cacheconfig")
  (sysctl-name "hw.cachelinesize_compat")
  (sysctl-name "hw.cpufamily")
  (sysctl-name "hw.cpufrequency_compat")
  (sysctl-name "hw.cputype")
  (sysctl-name "hw.l1dcachesize_compat")
  (sysctl-name "hw.l1icachesize_compat")
  (sysctl-name "hw.l2cachesize_compat")
  (sysctl-name "hw.l3cachesize_compat")
  (sysctl-name "hw.logicalcpu_max")
  (sysctl-name "hw.machine")
  (sysctl-name "hw.model")
  (sysctl-name "hw.memsize")
  (sysctl-name "hw.ncpu")
  (sysctl-name "hw.nperflevels")
  (sysctl-name-prefix "hw.optional.arm.")
  (sysctl-name-prefix "hw.optional.armv8_")
  (sysctl-name "hw.packages")
  (sysctl-name "hw.pagesize_compat")
  (sysctl-name "hw.pagesize")
  (sysctl-name "hw.physicalcpu")
  (sysctl-name "hw.physicalcpu_max")
  (sysctl-name "hw.logicalcpu")
  (sysctl-name "hw.cpufrequency")
  (sysctl-name "hw.tbfrequency_compat")
  (sysctl-name "hw.vectorunit")
  (sysctl-name "machdep.cpu.brand_string")
  (sysctl-name "kern.argmax")
  (sysctl-name "kern.hostname")
  (sysctl-name "kern.maxfilesperproc")
  (sysctl-name "kern.maxproc")
  (sysctl-name "kern.osproductversion")
  (sysctl-name "kern.osrelease")
  (sysctl-name "kern.ostype")
  (sysctl-name "kern.osvariant_status")
  (sysctl-name "kern.osversion")
  (sysctl-name "kern.secure_kernel")
  ; Python's ProcessPoolExecutor queries this through sysconf(_SC_SEM_NSEMS_MAX).
  (sysctl-name "kern.sysv.semmns")
  (sysctl-name "kern.usrstack64")
  (sysctl-name "kern.version")
  (sysctl-name "sysctl.proc_cputype")
  (sysctl-name "vm.loadavg")
  (sysctl-name-prefix "hw.perflevel")
  (sysctl-name-prefix "kern.proc.pgrp.")
  (sysctl-name-prefix "kern.proc.pid.")
)

; Java reads CPU info through a userspace buffer, misclassified as a write.
(allow sysctl-write
  (sysctl-name "kern.grade_cputype"))

(allow iokit-open
  (iokit-registry-entry-class "RootDomainUserClient"))

; User/group lookups (getpwuid and friends).
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo"))

; Python multiprocessing on macOS uses POSIX semaphores.
(allow ipc-posix-sem)

; PyTorch/libomp register OpenMP runtimes through these segments.
(allow ipc-posix-shm-read-data
  ipc-posix-shm-write-create
  ipc-posix-shm-write-unlink
  (ipc-posix-name-regex #"^/__KMP_REGISTERED_LIB_[0-9]+$"))

(allow mach-lookup
  (global-name "com.apple.PowerManagement.control"))

; openpty() so interactive children keep a TTY.
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-read* file-write*
  (require-all
    (regex #"^/dev/ttys[0-9]+")
    (extension "com.apple.sandbox.pty")))
; PTYs created before entering the seatbelt lack the extension above; ioctl
; keeps isatty() honest for them.
(allow file-ioctl (regex #"^/dev/ttys[0-9]+"))
"#;

/// The full-network allowance: rendered only when the route is [`NetworkRoute::Direct`]
/// (`networkAccess=on`, no allowlist). Under a deny-default base the absence
/// of an allowance IS a denial, so the closure (network off) simply stops
/// rendering this section instead of adding denies.
const SEATBELT_NETWORK_OPEN_POLICY: &str = r#"(allow network-outbound)
(allow network-inbound)
"#;

/// The platform minimums every networked route consults: only the safe
/// AF_SYSTEM sockets local platform services use, the DNS/TLS/user-directory
/// mach services the network stack touches per connection, and the
/// routing-table probes getaddrinfo sorts through. Rendered for
/// [`NetworkRoute::Direct`] and [`NetworkRoute::Managed`] (the proxy path
/// still resolves `localhost` and validates TLS to the proxy); omitted when
/// the network is off — a DNS lookup has no meaning with no socket to use.
const SEATBELT_NETWORK_PLATFORM_MINIMUMS: &str = r#"; Only the safe AF_SYSTEM sockets used by local platform services.
(allow system-socket
  (require-all
    (socket-domain AF_SYSTEM)
    (socket-protocol 2)))

; DNS configuration, TLS trust, and the user-directory lookups the network
; stack performs per connection.
(allow mach-lookup
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.system.opendirectoryd.membership")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.networkd")
  (global-name "com.apple.ocspd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.TrustEvaluationAgent")
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.SystemConfiguration.configd"))

; Routing-table probes (getaddrinfo sorts results through these).
(allow sysctl-read
  (sysctl-name-regex #"^net.routetable"))
"#;

/// Local unix-socket IPC, admitted in every network state: AF_UNIX
/// socketpairs are how build tools (`cargo clippy` and friends) manage child
/// processes, and a closed network must not take subprocess plumbing down
/// with it. Under the open route the blanket outbound allowance already
/// covers these; rendering them twice is harmless.
const SEATBELT_UNIX_IPC_POLICY: &str = r#"; Unix domain sockets for local IPC (subprocess management, not network).
(allow system-socket (socket-domain AF_UNIX))
(allow network-bind (local unix-socket))
(allow network-outbound (remote unix-socket))
"#;

/// Preference IPC: cfprefsd rides mach/shm outside the filesystem, and reads
/// are unrestricted in this phase, so its agent services stay reachable.
const SEATBELT_PREFERENCES_POLICY: &str = r#"(allow ipc-posix-shm-read*
  (ipc-posix-name-prefix "apple.cfprefs."))
(allow mach-lookup
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (local-name "com.apple.cfprefsd.agent"))
(allow user-preference-read)
"#;

/// Denies riding last in the profile (seatbelt resolves to the last matching
/// rule, so a trailing deny out-ranks any allowance above): XPC service
/// launches stay closed — the named `global-name` lookups above are the
/// entire mach surface — and the two fcntls that mutate a file through a
/// read-only descriptor (bypassing `file-write*` entirely) stay refused even
/// though deny-default alone would not catch them. F_MAKECOMPRESSED = 80,
/// F_TRANSFEREXTENTS = 110.
const SEATBELT_FINAL_DENIES: &str = r#"(deny mach-lookup (xpc-service-name-prefix ""))
(deny system-fcntl (fcntl-command 80 110))
"#;

/// Confinement policy for one sandboxed invocation: the workspace root whose
/// `writable_roots()` the `sandbox` profile allows, plus extra
/// granted roots (a same-repo worktree of the call's cwd admits itself;
/// an escalation-approved root accumulates for the session — additive on
/// top of the workspace, never a replacement). The effective mode is supplied per call
/// by the `mode_resolver` on [`SandboxedBashOperations`], not held here.
/// Construction validates every root spelling (symlinked roots are
/// rejected), so rendering stays infallible.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    workspace_root: PathBuf,
    extra_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// Build the project policy for `project_root`. The root is validated at
    /// its granted spelling, then canonicalized best-effort so the seatbelt
    /// `subpath` matching (which resolves symlinks) compares against real
    /// paths — the temp dir is a symlink to `/private/var/...` on macOS.
    pub fn for_project(project_root: &Path) -> Result<Self, String> {
        Ok(Self {
            workspace_root: validated_writable_root(project_root)?,
            extra_roots: Vec::new(),
        })
    }

    /// Add roots the `sandbox` profile admits on top of the workspace
    /// (validated, canonicalized, deduplicated). Full access never renders a
    /// profile — backend selection skips the seatbelt for it.
    pub fn with_extra_roots(mut self, roots: Vec<PathBuf>) -> Result<Self, String> {
        for root in roots {
            let canon = validated_writable_root(&root)?;
            if !self.extra_roots.iter().any(|r| r == &canon) {
                self.extra_roots.push(canon);
            }
        }
        Ok(self)
    }

    /// Render a seatbelt (`.sbpl`) policy string for the effective `mode`
    /// and network `route`. Deny-default base with the explicit allow set
    /// described in the module docs; `sandbox` admits the shared
    /// `writable_roots()` plus the extra granted roots. `full-access` is
    /// never rendered — backend selection skips the seatbelt for it — and
    /// rendering it anyway shows the profile alone cannot be trusted to
    /// unconfine.
    pub fn render_seatbelt(&self, mode: AccessMode, route: NetworkRoute) -> String {
        self.render_seatbelt_with_params(mode, route).0
    }

    /// The writable roots one call admits under `mode`: the shared
    /// `writable_roots()` plus the extra granted roots (both mode-gated,
    /// the filesystem root filtered out). One derivation for both
    /// enforcement faces — the seatbelt `-D` params and the Linux bwrap
    /// `--bind` list — so the root sets cannot fork across platforms.
    fn writable_root_set(&self, mode: AccessMode) -> Vec<PathBuf> {
        let mut roots = writable_roots(mode, &self.workspace_root)
            .into_iter()
            .filter(|r| r.parent().is_some())
            .collect::<Vec<_>>();
        // The extra granted roots ride the same mode gate as the workspace.
        if mode == AccessMode::Sandbox {
            for root in &self.extra_roots {
                if root.parent().is_some() && !roots.contains(root) {
                    roots.push(root.clone());
                }
            }
        }
        roots
    }

    /// The profile plus its `sandbox-exec -D` definitions, split so the argv
    /// builder and the tests see the same root→param mapping. The network
    /// sections follow the route: `Disabled` renders the unix-IPC minimum
    /// only (deny-default closes the rest), `Direct` renders the full open
    /// network with the platform minimums, `Managed` renders the loopback
    /// proxy shape.
    fn render_seatbelt_with_params(
        &self,
        mode: AccessMode,
        route: NetworkRoute,
    ) -> (String, Vec<(String, PathBuf)>) {
        let roots = self.writable_root_set(mode);
        let (write_section, params) = render_write_section(&roots);
        let network_section = render_network_section(route);
        let mut profile = String::new();
        profile.push_str(SEATBELT_BASE_POLICY);
        profile.push_str(&write_section);
        profile.push_str(&network_section);
        profile.push_str(SEATBELT_PREFERENCES_POLICY);
        profile.push_str(SEATBELT_FINAL_DENIES);
        (profile, params)
    }

    /// The `sandbox-exec` argv for `command` (single argv element to
    /// `bash -c` — no re-evaluation, no injection). One `-D` definition per
    /// writable root; the profile references them via `(param ...)`.
    /// Cross-platform so the seatbelt renderer (and its tests) compile on
    /// every target even though only macOS actually runs `sandbox-exec`.
    #[cfg(target_os = "macos")]
    pub fn wrap_argv(&self, command: &str, mode: AccessMode, route: NetworkRoute) -> Vec<String> {
        let (profile, params) = self.render_seatbelt_with_params(mode, route);
        let mut argv = vec!["-p".to_string(), profile];
        for (key, value) in params {
            argv.push(format!("-D{key}={}", value.display()));
        }
        argv.push("--".to_string());
        argv.extend(["bash".to_string(), "-c".to_string(), command.to_string()]);
        argv
    }

    /// The trapped-env signals the route injects on top of the confinement:
    /// the network self-check flag when closed, and the proxy env when the
    /// managed route is active (upper- and lowercase — clients differ in
    /// which they honor; a client that honors none fails closed at the
    /// profile).
    pub fn network_env(route: NetworkRoute) -> Vec<(&'static str, String)> {
        let mut env = Vec::new();
        if route == NetworkRoute::Disabled {
            env.push((
                manox_harness::sandbox::NETWORK_DISABLED_ENV,
                "1".to_string(),
            ));
        }
        if let NetworkRoute::Managed { proxy_port } = route {
            let url = format!("http://127.0.0.1:{proxy_port}");
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                env.push((key, url.clone()));
            }
        }
        env
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
        mode: AccessMode,
        route: NetworkRoute,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
        cmd.args(self.wrap_argv(command, mode, route))
            .env("PATH", login_shell_path())
            .current_dir(cwd);
        for (key, value) in Self::network_env(route) {
            cmd.env(key, value);
        }
        inject_noninteractive_env(&mut cmd);
        cmd
    }
}

/// The network sections one route renders, in profile order. `Disabled`
/// keeps only unix-socket IPC (subprocess plumbing survives; everything
/// else deny-default closes). `Direct` is the pre-closure full open with
/// the platform minimums. `Managed` admits loopback only — the proxy port
/// for tunnelled egress, loopback bind/inbound for local servers — with the
/// platform minimums and TLS trust kept (the client still resolves
/// `localhost` and validates the proxy TLS hop); a client that ignores the
/// proxy env finds no non-loopback allowance and fails closed at the OS.
fn render_network_section(route: NetworkRoute) -> String {
    let mut section = String::new();
    section.push_str(SEATBELT_UNIX_IPC_POLICY);
    match route {
        NetworkRoute::Disabled => {}
        NetworkRoute::Direct => {
            section.push_str(SEATBELT_NETWORK_OPEN_POLICY);
            section.push_str(SEATBELT_NETWORK_PLATFORM_MINIMUMS);
        }
        NetworkRoute::Managed { proxy_port } => {
            section.push_str(&format!(
                "; Managed egress: only the loopback proxy tunnel, plus loopback\n; bind/inbound for local servers.\n(allow network-bind (local ip \"*:*\"))\n(allow network-inbound (local ip \"localhost:*\"))\n(allow network-outbound (remote ip \"localhost:{proxy_port}\"))\n"
            ));
            section.push_str(SEATBELT_NETWORK_PLATFORM_MINIMUMS);
        }
    }
    section
}

/// The per-root write allowances plus the `-D` definitions they reference.
/// Each root gets an anchor deny on the root directory itself: renaming (or
/// otherwise unlinking) the boundary directory would move every protected
/// path out from under the policy that names it.
fn render_write_section(roots: &[PathBuf]) -> (String, Vec<(String, PathBuf)>) {
    let mut profile = String::new();
    let mut params = Vec::new();
    profile.push_str("; Writes only under the curated roots; each root directory itself is\n; anchored so it cannot be renamed out of the boundary.\n");
    for (index, root) in roots.iter().enumerate() {
        let key = format!("WRITABLE_ROOT_{index}");
        profile.push_str(&format!(
            "(allow file-write* (subpath (param \"{key}\")))\n"
        ));
        profile.push_str(&format!(
            "(deny file-write-unlink (require-all (literal (param \"{key}\")) (vnode-type DIRECTORY)))\n"
        ));
        params.push((key, root.clone()));
    }
    (profile, params)
}

/// Validate a writable root at the spelling the caller granted: fold the
/// top-level system alias (`/tmp` → `/private/tmp` on macOS — the platform's
/// own symlink is not user-controlled), then reject any other symlink in the
/// root's component chain. A symlinked root would make the profile reference
/// a name whose meaning someone else controls, so it fails closed instead of
/// silently granting (or silently missing) the resolved target.
fn validated_writable_root(raw: &Path) -> Result<PathBuf, String> {
    let normalized = normalize_top_level_alias(raw)?;
    if let Some(link) = nested_symlink_component(&normalized) {
        return Err(format!(
            "writable root {} contains the symlink {}; symlinked writable roots are \
             unsupported - grant the resolved path instead",
            raw.display(),
            link.display()
        ));
    }
    Ok(canonicalize_best_effort(raw))
}

/// Fold a top-level system alias into its real spelling: when the first path
/// component is a symlink (macOS `/tmp`, `/var`, `/etc`), canonicalize that
/// component and rejoin the suffix. Anything deeper is left for
/// [`nested_symlink_component`] to judge.
fn normalize_top_level_alias(path: &Path) -> Result<PathBuf, String> {
    let Some(top) = path.ancestors().find(|ancestor| {
        ancestor.parent().is_some() && ancestor.parent().and_then(Path::parent).is_none()
    }) else {
        return Ok(path.to_path_buf()); // the filesystem root itself
    };
    let Ok(meta) = std::fs::symlink_metadata(top) else {
        return Ok(path.to_path_buf()); // missing top level: nothing to fold
    };
    if !meta.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }
    let resolved = std::fs::canonicalize(top).map_err(|e| {
        format!(
            "failed to resolve the top-level alias {} in writable root {}: {e}",
            top.display(),
            path.display()
        )
    })?;
    let suffix = path.strip_prefix(top).unwrap_or(path);
    Ok(resolved.join(suffix))
}

/// The deepest symlink in `path`'s component chain, ignoring the top level
/// (direct children of `/` were already folded by
/// [`normalize_top_level_alias`]). Missing components never match.
fn nested_symlink_component(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| {
            let Ok(meta) = std::fs::symlink_metadata(ancestor) else {
                return false;
            };
            meta.file_type().is_symlink() && ancestor.parent().and_then(Path::parent).is_some()
        })
        .map(Path::to_path_buf)
}

/// Whether the OS sandbox backend is available on the current platform:
/// macOS needs `sandbox-exec`; Linux needs a usable non-setuid bubblewrap
/// with working unprivileged user namespaces (probed once per process).
/// Where neither exists the bash tool degrades to per-call approval.
pub fn is_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::path::Path::new("/usr/bin/sandbox-exec").exists()
    }
    #[cfg(target_os = "linux")]
    {
        linux::is_available()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
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
// Consumers: the seatbelt/bwrap wrap paths + the unsandboxed backend.
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

/// One-shot seatbelt-wrapped bash backend for the bash tool. The effective
/// mode is resolved per call by the host-injected `mode_resolver` (the
/// session mode, or an approved `require_escalated` grant for one call) —
/// the kernel `BashOperations` trait stays untouched, so the mode travels
/// through a closure, not a kernel field.
// On platforms without a backend the compiled stub's fields stay unread
// (`is_available()` is always false there, so the backend is never built).
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub struct SandboxedBashOperations {
    workspace: PathBuf,
    granted_roots: crate::granted_roots::GrantedRoots,
    mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync>,
    /// Host-injected network resolver: the session's `networkAccess` (plus
    /// the managed proxy port when one is running), read per call. Mirrors
    /// the `mode_resolver` injection pattern — the kernel trait stays
    /// untouched.
    network_resolver: Arc<dyn Fn() -> NetworkRoute + Send + Sync>,
}

impl SandboxedBashOperations {
    pub fn new(
        workspace: impl Into<PathBuf>,
        granted_roots: crate::granted_roots::GrantedRoots,
        mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync>,
        network_resolver: Arc<dyn Fn() -> NetworkRoute + Send + Sync>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            granted_roots,
            mode_resolver,
            network_resolver,
        }
    }

    /// The confinement policy for a call running in `cwd`: the session
    /// workspace profile plus the granted roots derived from that exact
    /// directory — a linked worktree of the workspace admits itself, an
    /// escalation-approved root stays for the session. The same derivation
    /// the fs fence applies, so a call one surface passed the other never
    /// denies. `Err` when a root spelling fails validation (symlinked
    /// writable roots) — the caller must fail the call closed.
    fn policy_for(&self, cwd: &Path) -> Result<SandboxPolicy, String> {
        SandboxPolicy::for_project(&self.workspace)?
            .with_extra_roots(self.granted_roots.roots_for(cwd))
    }

    /// Build the confined command for a background task — the same per-call
    /// mode and env as a foreground call (seatbelt on macOS, bwrap + seccomp
    /// on Linux), so a non-escalated background task is confined identically.
    /// The registry adds process-group/pipes after this. Unreachable stub on
    /// platforms without a backend: the wrap closure is only installed where
    /// `is_available()`.
    pub fn wrap_background(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        /// Fail-closed refusal for an unbuildable confinement: the task
        /// runs, prints a fixed refusal line, and exits nonzero — it never
        /// falls back to an unsandboxed run. No escalation hint rides here
        /// on purpose: a wider mode grants paths, it cannot fix a symlinked
        /// root (nor a vanished bwrap).
        fn refused(cwd: &Path) -> tokio::process::Command {
            let mut cmd = tokio::process::Command::new("bash");
            cmd.arg("-c")
                .arg(
                    "printf '%s\\n' 'sandbox: background task refused - a writable \
                     root contains a symlink component' >&2; exit 126",
                )
                .current_dir(cwd);
            cmd
        }
        #[cfg(target_os = "macos")]
        {
            let mode = (self.mode_resolver)();
            let route = (self.network_resolver)();
            match self.policy_for(cwd) {
                Ok(policy) => policy.wrap_command(command, cwd, mode, route),
                Err(_) => refused(cwd),
            }
        }
        #[cfg(target_os = "linux")]
        {
            let mode = (self.mode_resolver)();
            let route = (self.network_resolver)();
            match self.policy_for(cwd).and_then(|policy| {
                linux::wrap_command(&policy.writable_root_set(mode), command, cwd, route)
            }) {
                Ok(cmd) => cmd,
                Err(_) => refused(cwd),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = request;
            return Err(ExecutionError::Other(
                "the OS sandbox backend is not available on this platform".into(),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            let cwd = request
                .cwd
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.workspace.clone());
            let mode = (self.mode_resolver)();
            let route = (self.network_resolver)();
            // A policy that cannot be built (a symlinked writable root)
            // fails the call closed — the command never runs unsandboxed.
            let policy = self.policy_for(&cwd).map_err(ExecutionError::Other)?;
            let mut cmd = policy.wrap_command(request.command, &cwd, mode, route);
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let child = cmd
                .spawn()
                .map_err(|e| ExecutionError::Spawn(e.to_string()))?;
            let timeout = request.timeout.unwrap_or(DEFAULT_TIMEOUT);
            let mut result =
                run_to_completion(child, request.on_data, timeout, &request.signal).await?;
            // Classify a seatbelt denial — file-write EPERM, or a network
            // refusal when the route is closed — and surface the matching
            // marker + escalation hint so the model recognizes a policy
            // refusal (not a command bug) and can retry with
            // `require_escalated`.
            if let Some(note) = manox_harness::sandbox::seatbelt_denial_note(
                &result.stderr,
                result.exit_code,
                mode,
                route.access(),
            ) {
                result.stderr.push_str(&note);
            }
            Ok(result)
        }
        #[cfg(target_os = "linux")]
        {
            let cwd = request
                .cwd
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.workspace.clone());
            let mode = (self.mode_resolver)();
            let route = (self.network_resolver)();
            // Same fail-closed policy build as macOS (a symlinked writable
            // root never runs unsandboxed).
            let roots = self
                .policy_for(&cwd)
                .map_err(ExecutionError::Other)?
                .writable_root_set(mode);
            let mut cmd = linux::wrap_command(&roots, request.command, &cwd, route)
                .map_err(ExecutionError::Other)?;
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let child = cmd
                .spawn()
                .map_err(|e| ExecutionError::Spawn(e.to_string()))?;
            let timeout = request.timeout.unwrap_or(DEFAULT_TIMEOUT);
            let mut result =
                run_to_completion(child, request.on_data, timeout, &request.signal).await?;
            // Classify a bwrap denial — the read-only base surfaces EROFS
            // ("Read-only file system"), the socket-restricted seccomp face
            // EPERM — onto the same marker + escalation hint vocabulary,
            // re-worded for the network face when the route is closed.
            if let Some(note) = manox_harness::sandbox::seatbelt_denial_note(
                &result.stderr,
                result.exit_code,
                mode,
                route.access(),
            ) {
                result.stderr.push_str(&note);
            }
            Ok(result)
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
    on_data: Option<manox_harness::tools::bash::BashDataCallback<'_>>,
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
/// `full-access`; the same non-interactive git env and login PATH are
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

    /// The unconfined invocation for a command: `bash -c` with the login
    /// PATH and the non-interactive editor/pager env. Shared by this
    /// backend's `exec` and the background registry's escalated tasks, so an
    /// escalated command runs in the same shell and environment however it
    /// is launched.
    pub fn command(command: &str, cwd: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd.env("PATH", login_shell_path());
        inject_noninteractive_env(&mut cmd);
        cmd
    }
}

#[async_trait::async_trait]
impl BashOperations for UnsandboxedBashOperations {
    async fn exec(&self, request: BashExecRequest<'_>) -> Result<CommandResult, ExecutionError> {
        let cwd = request
            .cwd
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.base_cwd.clone());
        let mut cmd = Self::command(request.command, &cwd);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn project_root() -> PathBuf {
        canonicalize_best_effort(&std::env::temp_dir()).join("manox-sandbox-test-proj")
    }

    fn policy() -> SandboxPolicy {
        SandboxPolicy::for_project(&project_root()).unwrap()
    }

    fn param_paths(params: &[(String, PathBuf)]) -> Vec<PathBuf> {
        params.iter().map(|(_, p)| p.clone()).collect()
    }

    #[cfg(target_os = "macos")]
    fn resolver(mode: AccessMode) -> Arc<dyn Fn() -> AccessMode + Send + Sync> {
        Arc::new(move || mode)
    }

    #[cfg(target_os = "macos")]
    fn route_resolver(route: NetworkRoute) -> Arc<dyn Fn() -> NetworkRoute + Send + Sync> {
        Arc::new(move || route)
    }

    #[test]
    fn sandbox_profile_is_deny_default_with_param_roots() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let (sb, params) = SandboxPolicy::for_project(&root)
            .unwrap()
            .render_seatbelt_with_params(AccessMode::Sandbox, NetworkRoute::Direct);
        // The base flips to deny-default; everything after it is curated
        // allowance.
        assert!(sb.starts_with("(version 1)\n(deny default)\n"));
        // The writable roots travel as `-D` definitions and appear in the
        // profile only through `(param ...)` — no root path is embedded in
        // the profile text.
        let paths = param_paths(&params);
        assert!(
            paths.contains(&root),
            "workspace root is a param: {params:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("/tmp"))
                || paths.contains(&PathBuf::from("/private/tmp")),
            "/tmp is a param: {params:?}"
        );
        // The manox state home is admitted (plan files write without
        // escalation).
        if let Some(home) = manox_harness::sandbox::manox_home() {
            assert!(
                paths.contains(&canonicalize_best_effort(&home)),
                "manox home is a param: {params:?}"
            );
        }
        assert!(
            sb.contains("(allow file-write* (subpath (param \"WRITABLE_ROOT_0\")))"),
            "{sb}"
        );
        for (_, path) in &params {
            assert!(
                !sb.contains(&path.display().to_string()),
                "root path must not appear in the profile text: {sb}"
            );
        }
        // Each root is anchored: the root directory itself cannot be
        // unlinked (renamed out of the boundary).
        for (key, _) in &params {
            assert!(
                sb.contains(&format!(
                    "(deny file-write-unlink (require-all (literal (param \"{key}\")) \
                     (vnode-type DIRECTORY)))"
                )),
                "anchor deny for {key}: {sb}"
            );
        }
        // Explicit hardening denies survive any allowance above.
        assert!(sb.contains("(deny system-fcntl (fcntl-command 80 110))"));
        assert!(sb.contains("(deny mach-lookup (xpc-service-name-prefix \"\"))"));
    }

    #[test]
    fn full_access_never_renders_a_profile_here_but_the_base_still_denies() {
        // The deny-default base is mode-independent (hardening is a later
        // phase); `full-access` never reaches this renderer (backend
        // selection skips the seatbelt), but rendering it anyway shows the
        // profile alone cannot be trusted to unconfine.
        let (sb, params) =
            policy().render_seatbelt_with_params(AccessMode::FullAccess, NetworkRoute::Direct);
        assert!(sb.contains("(deny default)"));
        assert!(params.is_empty(), "full access admits no roots: {params:?}");
        assert!(!sb.contains("(allow file-write* (subpath"));
        assert!(sb.contains("(path \"/dev/null\")"));
        // Network allowances are explicit even with no roots admitted.
        assert!(sb.contains("(allow network-outbound)"));
    }

    #[test]
    fn network_is_explicitly_open_with_the_platform_whitelist() {
        // Phase 1 keeps the network fully open, but the allowance is
        // explicit under the deny-default base; the loopback-only closure is
        // phase 3 and shrinks this section. The platform minimums (DNS
        // configuration, TLS trust mach services) ride with it.
        let (sb, _) =
            policy().render_seatbelt_with_params(AccessMode::Sandbox, NetworkRoute::Direct);
        assert!(sb.contains("(allow network-outbound)"));
        assert!(sb.contains("(allow network-inbound)"));
        assert!(sb.contains("(socket-domain AF_SYSTEM)"));
        assert!(sb.contains("com.apple.SystemConfiguration.DNSConfiguration"));
        assert!(sb.contains("com.apple.TrustEvaluationAgent"));
        assert!(sb.contains("(sysctl-name-regex #\"^net.routetable\")"));
        // No network deny anywhere yet, and still no `.git` protection
        // (outside the mode vocabulary).
        assert!(!sb.contains("(deny network"));
        assert!(!sb.contains(".git"));
    }

    #[test]
    fn closed_network_renders_no_network_allowance() {
        // networkAccess=off: no outbound/inbound allowance anywhere (the
        // deny-default base is the enforcement), no DNS/TLS mach minimums,
        // no AF_SYSTEM — but unix-socket IPC stays so build-tool subprocess
        // plumbing survives.
        let (sb, _) =
            policy().render_seatbelt_with_params(AccessMode::Sandbox, NetworkRoute::Disabled);
        // The blanket allowance is gone (the unix-socket outbound line
        // carries a qualifier and stays).
        assert!(!sb.contains("(allow network-outbound)\n"), "{sb}");
        assert!(!sb.contains("(allow network-inbound)"), "{sb}");
        assert!(!sb.contains("socket-domain AF_SYSTEM"), "{sb}");
        assert!(
            !sb.contains("com.apple.SystemConfiguration.DNSConfiguration"),
            "{sb}"
        );
        assert!(!sb.contains("com.apple.trustd.agent"), "{sb}");
        assert!(!sb.contains("net.routetable"), "{sb}");
        // The unix IPC minimum rides in every state.
        assert!(sb.contains("(socket-domain AF_UNIX)"), "{sb}");
        assert!(
            sb.contains("(allow network-outbound (remote unix-socket))"),
            "{sb}"
        );
        // The writes and hardening denies are untouched.
        assert!(sb.contains("(allow file-write* (subpath (param \"WRITABLE_ROOT_0\")))"));
        assert!(sb.contains("(deny system-fcntl (fcntl-command 80 110))"));
    }

    #[test]
    fn managed_network_renders_the_loopback_proxy_shape() {
        // The managed route: loopback only — the proxy port for tunnelled
        // egress, loopback bind/inbound for local servers — with the
        // platform minimums; no blanket outbound allowance survives.
        let (sb, _) = policy().render_seatbelt_with_params(
            AccessMode::Sandbox,
            NetworkRoute::Managed { proxy_port: 54321 },
        );
        assert!(!sb.contains("(allow network-outbound)\n"), "{sb}");
        assert!(
            sb.contains("(allow network-outbound (remote ip \"localhost:54321\"))"),
            "{sb}"
        );
        assert!(
            sb.contains("(allow network-bind (local ip \"*:*\"))"),
            "{sb}"
        );
        assert!(
            sb.contains("(allow network-inbound (local ip \"localhost:*\"))"),
            "{sb}"
        );
        // The platform minimums and TLS trust stay (the proxy hop itself
        // resolves localhost and validates TLS).
        assert!(sb.contains("com.apple.SystemConfiguration.DNSConfiguration"));
        assert!(sb.contains("com.apple.TrustEvaluationAgent"));
    }

    #[test]
    fn network_env_signals_the_route() {
        use manox_harness::sandbox::NETWORK_DISABLED_ENV;
        let disabled = SandboxPolicy::network_env(NetworkRoute::Disabled);
        assert!(
            disabled
                .iter()
                .any(|(k, v)| *k == NETWORK_DISABLED_ENV && v == "1"),
            "{disabled:?}"
        );
        assert!(SandboxPolicy::network_env(NetworkRoute::Direct).is_empty());
        let managed = SandboxPolicy::network_env(NetworkRoute::Managed { proxy_port: 54321 });
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "https_proxy"] {
            assert!(
                managed
                    .iter()
                    .any(|(k, v)| *k == key && v == "http://127.0.0.1:54321"),
                "{managed:?}"
            );
        }
        // The disabled env never rides a proxy route, and the proxy env
        // never rides a closed one.
        assert!(!managed.iter().any(|(k, _)| *k == NETWORK_DISABLED_ENV));
        assert!(!disabled.iter().any(|(k, _)| k.contains("PROXY")));
    }

    #[test]
    fn route_access_maps_the_classifier_vocabulary() {
        assert_eq!(NetworkRoute::Disabled.access(), NetworkAccess::Off);
        assert_eq!(NetworkRoute::Direct.access(), NetworkAccess::On);
        assert_eq!(
            NetworkRoute::Managed { proxy_port: 1 }.access(),
            NetworkAccess::On
        );
        assert_eq!(
            NetworkRoute::from_access(NetworkAccess::Off),
            NetworkRoute::Disabled
        );
        assert_eq!(
            NetworkRoute::from_access(NetworkAccess::On),
            NetworkRoute::Direct
        );
    }

    #[test]
    fn filesystem_root_is_filtered_out() {
        // Launching from `/` must not admit the entire disk: the fs root is
        // filtered from the writable allow-list (the temp areas and the
        // manox home still ride along).
        let (sb, params) = SandboxPolicy::for_project(Path::new("/"))
            .unwrap()
            .render_seatbelt_with_params(AccessMode::Sandbox, NetworkRoute::Direct);
        assert!(
            !param_paths(&params).contains(&PathBuf::from("/")),
            "fs root not admitted: {params:?}"
        );
        assert!(!sb.contains("(subpath \"/\")"), "{sb}");
    }

    #[test]
    fn extra_roots_render_under_sandbox_only() {
        let wt = project_root().join("wt");
        let git = project_root().join(".git");
        let policy = SandboxPolicy::for_project(&project_root())
            .unwrap()
            .with_extra_roots(vec![wt.clone(), git.clone(), wt.clone()])
            .unwrap();
        let (_, params) =
            policy.render_seatbelt_with_params(AccessMode::Sandbox, NetworkRoute::Direct);
        let paths = param_paths(&params);
        for root in [&wt, &git] {
            assert!(
                paths.contains(&canonicalize_best_effort(root)),
                "extra root admitted: {params:?}"
            );
        }
        // Deduplicated: the duplicated `wt` yields one param.
        assert_eq!(
            paths
                .iter()
                .filter(|p| **p == canonicalize_best_effort(&wt))
                .count(),
            1,
            "{params:?}"
        );
        // Full access never admits anything.
        let (_, full) =
            policy.render_seatbelt_with_params(AccessMode::FullAccess, NetworkRoute::Direct);
        assert!(full.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_writable_roots_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // A symlinked root fails construction — for the workspace spelling
        // and for an extra granted root alike (fail-closed, no opt-in).
        let err = SandboxPolicy::for_project(&link).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        let err = SandboxPolicy::for_project(&target)
            .unwrap()
            .with_extra_roots(vec![link.clone()])
            .unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn top_level_system_aliases_are_normalized_not_rejected() {
        // `/tmp` is the platform's own top-level symlink; it folds to
        // `/private/tmp` instead of being refused.
        assert!(SandboxPolicy::for_project(Path::new("/tmp")).is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn background_wrap_fails_closed_on_an_unbuildable_policy() {
        // A symlinked workspace cannot build a policy; the background wrap
        // returns a refusal command (plain bash, not an unsandboxed ride of
        // the original command through sandbox-exec).
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let ops = SandboxedBashOperations::new(
            &link,
            crate::granted_roots::GrantedRoots::new(&target),
            resolver(AccessMode::Sandbox),
            route_resolver(NetworkRoute::Direct),
        );
        let cmd = ops.wrap_background("echo hi", &target);
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "bash");
        let script = std_cmd.get_args().nth(1).unwrap().to_string_lossy();
        assert!(
            script.contains("refused") && script.contains("exit 126"),
            "{script}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_argv_shapes_the_invocation() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let policy = SandboxPolicy::for_project(&root).unwrap();
        let args = policy.wrap_argv("echo hi", AccessMode::Sandbox, NetworkRoute::Direct);
        assert_eq!(args[0], "-p");
        assert!(args[1].contains("(deny default)"));
        // One `-D` definition per writable root, workspace root first, all
        // before the `--` separator.
        assert_eq!(
            args[2],
            format!(
                "-DWRITABLE_ROOT_0={}",
                canonicalize_best_effort(&root).display()
            )
        );
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert!(sep >= 3);
        assert!(
            args[2..sep]
                .iter()
                .all(|a| a.starts_with("-DWRITABLE_ROOT_")),
            "{args:?}"
        );
        assert_eq!(args[sep + 1], "bash");
        assert_eq!(args[sep + 2], "-c");
        assert_eq!(args[sep + 3], "echo hi", "command is a single argv element");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandboxed_exec_runs_and_confines_writes() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let ops = SandboxedBashOperations::new(
            &root,
            crate::granted_roots::GrantedRoots::new(&root),
            resolver(AccessMode::Sandbox),
            route_resolver(NetworkRoute::Direct),
        );

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
    async fn sandboxed_exec_admits_manox_home_writes() {
        let Some(home) = manox_harness::sandbox::manox_home() else {
            return; // no home dir to admit
        };
        std::fs::create_dir_all(&home).ok();
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let ops = SandboxedBashOperations::new(
            &root,
            crate::granted_roots::GrantedRoots::new(&root),
            resolver(AccessMode::Sandbox),
            route_resolver(NetworkRoute::Direct),
        );

        // The manox state home is writable under sandbox (plan files
        // and scratch state need no escalation).
        let probe = home.join("manox-sandbox-home-probe.txt");
        let _ = std::fs::remove_file(&probe);
        let req = BashExecRequest {
            command: &format!("echo hi > {}", probe.display()),
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let res = ops.exec(req).await.expect("exec runs");
        if res.exit_code == 71 && res.stderr.contains("Operation not permitted") {
            // Nested sandbox-exec is refused when the PARENT process is
            // itself sandboxed. Environment-specific — CI and the GUI app
            // are unsandboxed and exercise the real confinement.
            eprintln!("skipping seatbelt assertions: parent process sandboxed");
            return;
        }
        assert_eq!(
            res.exit_code, 0,
            "manox home write admitted: {}",
            res.stderr
        );
        assert!(probe.exists(), "probe written under the manox home");
        let _ = std::fs::remove_file(&probe);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_seatbelt_denies_out_of_root_writes_but_allows_reads() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let ops = SandboxedBashOperations::new(
            &root,
            crate::granted_roots::GrantedRoots::new(&root),
            resolver(AccessMode::Sandbox),
            route_resolver(NetworkRoute::Direct),
        );

        // A write outside the workspace + temp areas is denied under sandbox.
        let denied = PathBuf::from("/usr/local/manox-sandbox-deny-target");
        let req = BashExecRequest {
            command: &format!("touch {}", denied.display()),
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
        assert_ne!(res.exit_code, 0, "sandbox denies the write");
        // The seatbelt denial carries the denial marker + escalation hint
        // so the model recognizes a policy refusal (not a command bug).
        assert!(
            res.stderr
                .contains("[sandbox: file access denied under sandbox mode]"),
            "denial marker in stderr: {}",
            res.stderr
        );
        assert!(!denied.exists());

        // A read succeeds (reads are unrestricted in every mode).
        let req = BashExecRequest {
            command: "ls /dev/null",
            cwd: Some(&root),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        };
        let res = ops.exec(req).await.expect("read runs");
        assert_eq!(
            res.exit_code, 0,
            "read succeeds under sandbox: {}",
            res.stderr
        );
    }

    // ---- live: real seatbelt runs against the real network ----
    // (skip loudly when the parent process is itself sandboxed — the same
    // guard the confinement tests use.)

    async fn live_exec(root: &Path, command: &str, route: NetworkRoute) -> CommandResult {
        let ops = SandboxedBashOperations::new(
            root,
            crate::granted_roots::GrantedRoots::new(root),
            resolver(AccessMode::Sandbox),
            route_resolver(route),
        );
        use manox_harness::tools::bash::BashOperations as _;
        ops.exec(BashExecRequest {
            command,
            cwd: Some(root),
            timeout: Some(Duration::from_secs(60)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        })
        .await
        .expect("exec runs")
    }

    fn nested_sandboxed(result: &CommandResult) -> bool {
        result.exit_code == 71 && result.stderr.contains("Operation not permitted")
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn live_network_off_denies_curl_with_the_network_marker() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        // `-sS`: silence the progress meter but keep the error text — the
        // classifier reads stderr, and a bare `-s` would swallow the very
        // failure this test pins.
        let res = live_exec(
            &root,
            "curl -sS -I https://crates.io",
            NetworkRoute::Disabled,
        )
        .await;
        if nested_sandboxed(&res) {
            eprintln!("skipping live seatbelt test: parent process sandboxed");
            return;
        }
        assert_ne!(res.exit_code, 0, "closed network denies curl");
        // macOS curl has two phrasings for the refused connect ("Could not
        // connect to server" / "Couldn't connect to server") — either keeps
        // the failure classifiable.
        let lower = res.stderr.to_ascii_lowercase();
        assert!(
            lower.contains("could not connect") || lower.contains("couldn't connect"),
            "the connect failure is visible in stderr: {}",
            res.stderr
        );
        assert!(
            res.stderr
                .contains("network access denied under sandbox mode"),
            "network marker in stderr: {}",
            res.stderr
        );
        assert!(
            res.stderr.contains("require_escalated"),
            "escalation hint rides the denial: {}",
            res.stderr
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn live_network_off_signals_the_self_check_env() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let res = live_exec(
            &root,
            &format!(
                "test \"${}\" = 1",
                manox_harness::sandbox::NETWORK_DISABLED_ENV
            ),
            NetworkRoute::Disabled,
        )
        .await;
        if nested_sandboxed(&res) {
            eprintln!("skipping live seatbelt test: parent process sandboxed");
            return;
        }
        assert_eq!(
            res.exit_code, 0,
            "the self-check env is set when closed: {}",
            res.stderr
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn live_network_on_reaches_the_network_directly() {
        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let res = live_exec(&root, "curl -sI https://crates.io", NetworkRoute::Direct).await;
        if nested_sandboxed(&res) {
            eprintln!("skipping live seatbelt test: parent process sandboxed");
            return;
        }
        assert_eq!(
            res.exit_code, 0,
            "the direct route reaches the network: {}",
            res.stderr
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn live_managed_route_tunnels_the_allowlisted_host_and_refuses_misses() {
        use crate::sandbox::netpolicy::NetworkPolicy;
        use manox_harness::sandbox::NetworkAccess;

        let root = project_root();
        std::fs::create_dir_all(&root).ok();
        let policy = NetworkPolicy::for_allowlist(vec!["crates.io".to_string()]);
        policy.set_enabled(NetworkAccess::On).await;
        let route = policy.route_for(NetworkAccess::On);
        let NetworkRoute::Managed { proxy_port: _ } = route else {
            panic!("an allowlisted policy binds the managed route, got {route:?}");
        };
        let policy_for_resolver = policy.clone();
        let ops = SandboxedBashOperations::new(
            &root,
            crate::granted_roots::GrantedRoots::new(&root),
            resolver(AccessMode::Sandbox),
            Arc::new(move || policy_for_resolver.route_for(NetworkAccess::On)),
        );
        use manox_harness::tools::bash::BashOperations as _;

        // Allowlisted: the tunnel carries it.
        let res = ops
            .exec(BashExecRequest {
                command: "curl -sI https://crates.io",
                cwd: Some(&root),
                timeout: Some(Duration::from_secs(60)),
                signal: tokio_util::sync::CancellationToken::new(),
                on_data: None,
            })
            .await
            .expect("exec runs");
        if nested_sandboxed(&res) {
            eprintln!("skipping live seatbelt test: parent process sandboxed");
            return;
        }
        assert_eq!(
            res.exit_code, 0,
            "allowlisted host tunnels through the managed proxy: {} / {}",
            res.stdout, res.stderr
        );

        // A miss: the proxy refuses (no approver installed) and curl sees
        // the tunnel refusal.
        let res = ops
            .exec(BashExecRequest {
                command: "curl -sI https://example.com",
                cwd: Some(&root),
                timeout: Some(Duration::from_secs(60)),
                signal: tokio_util::sync::CancellationToken::new(),
                on_data: None,
            })
            .await
            .expect("exec runs");
        assert_ne!(res.exit_code, 0, "a non-allowlisted host is refused");
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
