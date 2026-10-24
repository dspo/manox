//! The Linux sandbox backend: bubblewrap confinement with the seccomp
//! filter installed by an inner helper — the host binary re-executing
//! itself inside the sandbox ([`super::inner`]).
//!
//! Two-phase spawn. The host forks, sets `NO_NEW_PRIVS` in the `pre_exec`
//! hook, and execs bubblewrap; bubblewrap builds the user/network/pid
//! namespaces and the mount table, then execs the host binary again with
//! the sentinel environment, where a `.init_array` constructor dispatches
//! into the inner helper. The inner installs the seccomp face for the route
//! and — under the managed route — binds the netns loopback listener and
//! hands it back to the host ([`super::bridge`]) before the target command
//! runs. The target only ever executes if the inner completed; a broken
//! helper fails the call closed.
//!
//! The filter cannot stay in the host's `pre_exec` (the first-phase shape):
//! bubblewrap's own network-namespace bring-up rides the netlink syscalls
//! the closed face denies, so the single-phase filter had to leave
//! `bind`/`sendto`/`sendmsg`/`recvmsg` and the netlink socket family
//! allowed. Moving the filter after bubblewrap's setup puts every one of
//! those back on the deny list. `NO_NEW_PRIVS` still installs in
//! `pre_exec` — it costs nothing for a non-setuid bubblewrap (setuid builds
//! are rejected by the probe anyway) and keeps the privilege floor under
//! the whole tree.
//!
//! Under the managed route the namespace IS the wall: the netns has no
//! external interface, the inner's loopback listener is the only egress,
//! and the host relays it to the session's CONNECT proxy
//! ([`super::bridge`]). The managed seccomp face therefore only gates the
//! socket families (inet for the bridge hop, unix for subprocess plumbing;
//! vsock/netlink and everything else denied) — an address-based filter is
//! not expressible in seccomp, and does not need to be.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::NetworkRoute;
use super::inner::{self, NetMode};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The confinement binary searched on PATH.
const BWRAP_PROGRAM: &str = "bwrap";

/// Ceiling for the availability smoke test — a healthy bwrap answers in
/// milliseconds; anything slower means the environment cannot host the
/// sandbox reliably.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(2);

/// A probed-and-usable bubblewrap.
#[derive(Debug, Clone)]
struct BwrapProbe {
    /// Absolute path (the child's exec is direct — no PATH search after
    /// `NO_NEW_PRIVS` is set).
    program: PathBuf,
    /// `--as-pid-1` doubles as the version floor: it appears in `--help`
    /// only on builds new enough for the pid-namespace shape we rely on.
    supports_as_pid_1: bool,
    /// `--preserve-fd` appears on builds that close inherited descriptors
    /// above stdio; such builds need the bridge descriptor named
    /// explicitly, older builds pass it through untouched (verified against
    /// 0.9.0). Probed defensively so a future sanitizing build degrades to
    /// a working managed route instead of a dead channel.
    supports_preserve_fd: bool,
}

/// The process-lifetime probe result: `Some` when bwrap is present,
/// non-setuid, feature-complete, and passes a real confinement smoke test.
static PROBE: OnceLock<Option<BwrapProbe>> = OnceLock::new();

/// Whether the Linux backend is usable here (probed once, cached).
pub(super) fn is_available() -> bool {
    probe().is_some()
}

/// Whether the managed route can ride the inner-helper bridge here: the
/// host binary must be resolvable, because the inner IS the host binary
/// re-executed. The network policy consults this before binding the proxy,
/// so an unresolvable path fails closed (`Disabled`) instead of binding a
/// proxy the trapped calls cannot reach.
pub(super) fn inner_ready() -> bool {
    std::env::current_exe().is_ok()
}

/// The cached probe. First call pays for the PATH search, the `--help`
/// feature read, and one real `bwrap` smoke run; every later call reads the
/// verdict.
fn probe() -> &'static Option<BwrapProbe> {
    PROBE.get_or_init(|| {
        let probe = run_probe();
        if probe.is_none() {
            tracing::warn!(
                "bubblewrap unavailable or unusable - sandboxed bash degrades to per-call approval"
            );
        }
        probe
    })
}

fn run_probe() -> Option<BwrapProbe> {
    let program = find_bwrap_on_path()?;
    // Feature flags ride the help text: `--as-pid-1` is the version floor
    // (bubblewrap gained it in 0.8), `--preserve-fd` marks builds that
    // close inherited descriptors above stdio.
    let output = std::process::Command::new(&program)
        .arg("--help")
        .output()
        .ok()?;
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let supports_as_pid_1 = help.contains("--as-pid-1");
    if !supports_as_pid_1 {
        return None;
    }
    let supports_preserve_fd = help.contains("--preserve-fd");
    // The kernel must actually grant the confinement: unprivileged user
    // namespaces are a boot-time or LSM policy (kernel.unprivileged_userns_
    // clone, apparmor userns restrictions), and only a real run can tell.
    smoke_test(&program)?;
    Some(BwrapProbe {
        program,
        supports_as_pid_1,
        supports_preserve_fd,
    })
}

/// First executable non-setuid `bwrap` on PATH, as an absolute path. A
/// setuid bwrap is skipped on purpose: the pre-exec `NO_NEW_PRIVS` strips
/// setuid elevation, so a setuid build would fail at runtime — and treating
/// it as usable would trade a silent breakage for the illusion of
/// confinement.
fn find_bwrap_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(BWRAP_PROGRAM);
        let Ok(meta) = std::fs::metadata(&candidate) else {
            return None;
        };
        if !meta.is_file() || !is_executable(meta.mode()) || has_setuid_bit(meta.mode()) {
            return None;
        }
        // Absolute spelling: exec after the filter installs must not search
        // PATH again.
        std::fs::canonicalize(&candidate).ok()
    })
}

fn is_executable(mode: u32) -> bool {
    mode & 0o111 != 0
}

/// The setuid bit (`S_ISUID`) — disqualifying under `NO_NEW_PRIVS`.
fn has_setuid_bit(mode: u32) -> bool {
    mode & 0o4000 != 0
}

/// Real-availability smoke: a minimal confinement running `/bin/true`
/// (ro-bind the root, a device tree, one unprivileged user namespace).
/// Any failure — userns disabled, LSM refusal, seccomp-profile interference
/// — lands as `None`; the message keywords distinguish the userns denials
/// for the log.
fn smoke_test(program: &Path) -> Option<()> {
    let mut child = std::process::Command::new(program)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-user",
            "--",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + SMOKE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        use std::io::Read as _;
                        let mut buf = Vec::new();
                        s.read_to_end(&mut buf).ok();
                        String::from_utf8_lossy(&buf).into_owned()
                    })
                    .unwrap_or_default();
                if status.success() {
                    return Some(());
                }
                tracing::warn!(
                    "bubblewrap smoke test failed (userns disabled: {}): {stderr}",
                    stderr_namespaces_are_denied(&stderr)
                );
                return None;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!("bubblewrap smoke test timed out");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                tracing::warn!("bubblewrap smoke test wait failed: {e}");
                return None;
            }
        }
    }
}

/// Whether a bwrap failure message names the user-namespace denials
/// (`/proc/sys/kernel/unprivileged_userns_clone` off, apparmor userns
/// restriction, ...) — the common "installed but this kernel says no" shape.
fn stderr_namespaces_are_denied(stderr: &str) -> bool {
    const DENIALS: [&str; 2] = [
        "setting up uid map: Permission denied",
        "No permissions to create a new namespace",
    ];
    DENIALS.iter().any(|d| stderr.contains(d))
}

/// The bwrap argv for `command` (a single argv element handed to
/// `bash -c` — no re-evaluation, no injection), confined to `roots`:
/// read-only root filesystem, a minimal writable `/dev` with the host shm
/// re-attached, then every writable root re-bound read-write over the
/// read-only base. The user namespace is unshared explicitly (root-inside-
/// a-container needs no ambient capabilities to create the rest), the pid
/// namespace with bwrap itself as its pid 1 when the build supports it
/// (signals reach bash, and the namespace dies with the direct child the
/// parent manages). The network namespace follows the route: `Disabled`
/// and `Managed` both unshare it — closed by construction for the former,
/// the managed route's isolation wall for the latter (the inner's loopback
/// listener is the only egress, relayed by the host bridge) — while the
/// direct route shares the host's. After the mounts, the inner helper
/// launch: the host binary again (`inner_exe`), then the target command.
pub(super) fn bwrap_argv(
    roots: &[PathBuf],
    inner_exe: &Path,
    command: &str,
    supports_as_pid_1: bool,
    supports_preserve_fd: bool,
    route: NetworkRoute,
    bridge_fd: Option<libc::c_int>,
) -> Vec<String> {
    let mut argv: Vec<String> = [
        "--ro-bind",
        "/",
        "/",
        // `--dev /dev` lays the standard minimal nodes (null, zero, random,
        // urandom, tty, ...) over the read-only base; the host shm rides
        // along so POSIX-semaphore runtimes (python multiprocessing, libomp)
        // keep working — the seatbelt admits the same effect via
        // `ipc-posix-shm`.
        "--dev",
        "/dev",
        "--bind-try",
        "/dev/shm",
        "/dev/shm",
        "--new-session",
        "--die-with-parent",
        "--unshare-user",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if route != NetworkRoute::Direct {
        argv.push("--unshare-net".into());
    }
    for root in roots {
        if root.exists() {
            argv.push("--bind".into());
            argv.push(root.display().to_string());
            argv.push(root.display().to_string());
        }
    }
    if supports_as_pid_1 {
        argv.push("--unshare-pid".into());
        argv.push("--as-pid-1".into());
        // A fresh procfs so /proc reflects the new pid namespace — the
        // read-only base mount would keep showing host processes the
        // sandboxed shell cannot signal.
        argv.push("--proc".into());
        argv.push("/proc".into());
    }
    // Belt for bubblewrap builds that sanitize descriptors above stdio:
    // name the bridge channel explicitly. Builds without the flag (0.9.0
    // among them) pass inherited descriptors through untouched.
    if let (Some(fd), true) = (bridge_fd, supports_preserve_fd) {
        argv.push("--preserve-fd".into());
        argv.push(fd.to_string());
    }
    argv.push("--".into());
    argv.push(inner_exe.display().to_string());
    argv.push("--".into());
    argv.push("bash".into());
    argv.push("-c".into());
    argv.push(command.to_string());
    argv
}

/// The syscalls the deny face refuses (default action allow, verdict
/// EPERM). `ptrace` and the `process_vm_*` cross-process memory family are
/// the classic sandbox-escape primitives against the host process itself
/// (the filter is installed before the target runs, so a sandboxed child
/// could otherwise attach back to its unsandboxed ancestors); `io_uring`
/// bypasses the standard syscall path for its submitted operations, which
/// would let any network closure be sidestepped — it stays unavailable in
/// every network state.
const DENIED_SYSCALLS: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// The socket-lifecycle syscalls the closed face refuses in full. The
/// filter installs after bubblewrap's netns bring-up, so unlike the
/// single-phase shape the data-plane verbs (`sendto`, `sendmsg`,
/// `recvmsg`, `bind`) and the option calls go back on the deny list —
/// the netns took over the loopback bring-up. `recvfrom` stays allowed:
/// socketpair-based subprocess management (`cargo clippy` and friends)
/// rides it.
const NET_DENIED_SYSCALLS: &[i64] = &[
    libc::SYS_connect,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_getpeername,
    libc::SYS_getsockname,
    libc::SYS_shutdown,
    libc::SYS_sendto,
    libc::SYS_sendmsg,
    libc::SYS_sendmmsg,
    libc::SYS_recvmsg,
    libc::SYS_recvmmsg,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
];

/// The deny rules for one inner face. The closed face (`Off`) refuses the
/// whole socket lifecycle and leaves `socket`/`socketpair` AF_UNIX-only —
/// netlink included in the denial, since nothing but bubblewrap's own
/// bring-up ever needed it and that finished before the filter landed. The
/// managed face keeps `connect` open — seccomp cannot filter by address,
/// and the netns reduces the reachable address space to loopback — and
/// gates the socket families instead: inet for the bridge hop, unix for
/// local IPC; vsock (which ignores network namespaces and would reach the
/// host) and every other family stay denied. The direct face carries only
/// the escape primitives.
fn deny_rules(mode: NetMode) -> BTreeMap<i64, Vec<SeccompRule>> {
    let mut rules = BTreeMap::new();
    for &nr in DENIED_SYSCALLS {
        rules.insert(nr, Vec::new());
    }
    match mode {
        NetMode::Direct => {}
        NetMode::Off => {
            for &nr in NET_DENIED_SYSCALLS {
                rules.insert(nr, Vec::new());
            }
            rules.insert(libc::SYS_socket, vec![unix_domain_rule()]);
            rules.insert(libc::SYS_socketpair, vec![unix_domain_rule()]);
        }
        NetMode::Managed => {
            rules.insert(libc::SYS_socket, vec![inet_or_unix_domain_rule()]);
            rules.insert(libc::SYS_socketpair, vec![unix_domain_rule()]);
        }
    }
    rules
}

/// Denied only when the domain argument is not AF_UNIX; the local case
/// falls through to the default allow.
fn unix_domain_rule() -> SeccompRule {
    SeccompRule::new(vec![domain_is_not(libc::AF_UNIX)]).expect("unix-domain rule compiles")
}

/// Denied for every domain except the managed face's three locals: AF_UNIX
/// (subprocess plumbing), AF_INET and AF_INET6 (the netns loopback bridge).
fn inet_or_unix_domain_rule() -> SeccompRule {
    SeccompRule::new(vec![
        domain_is_not(libc::AF_INET),
        domain_is_not(libc::AF_INET6),
        domain_is_not(libc::AF_UNIX),
    ])
    .expect("inet-or-unix domain rule compiles")
}

/// One `domain != value` comparison on the first syscall argument.
fn domain_is_not(value: libc::c_int) -> SeccompCondition {
    SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, value as u64)
        .expect("domain comparison compiles")
}

fn seccomp_target_arch() -> Option<TargetArch> {
    if cfg!(target_arch = "x86_64") {
        Some(TargetArch::x86_64)
    } else if cfg!(target_arch = "aarch64") {
        Some(TargetArch::aarch64)
    } else {
        None
    }
}

/// Compile the BPF program for `mode`: allow-by-default with EPERM on the
/// deny face. `Err` only where the architecture is outside the supported
/// set. The inner helper calls this in its own process (once per run);
/// the host keeps no compiled program anymore — the filter's only applier
/// is the inner.
pub(super) fn compile_program(mode: NetMode) -> Result<BpfProgram, String> {
    let Some(arch) = seccomp_target_arch() else {
        return Err(
            "seccomp filter unsupported on this architecture - the Linux sandbox \
             needs x86_64 or aarch64"
                .into(),
        );
    };
    let filter = SeccompFilter::new(
        deny_rules(mode),
        SeccompAction::Allow,                     // default: allow
        SeccompAction::Errno(libc::EPERM as u32), // denied syscall: EPERM
        arch,
    )
    .map_err(|e: seccompiler::BackendError| e.to_string())?;
    let program: BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| e.to_string())?;
    Ok(program)
}

/// Compile and apply the face in the inner helper (the filter's only
/// home). `apply_filter` sets `NO_NEW_PRIVS` itself; the flag already
/// rides from the host's `pre_exec` and is idempotent.
pub(super) fn install_seccomp(mode: NetMode) -> Result<(), String> {
    let program = compile_program(mode)?;
    seccompiler::apply_filter(&program).map_err(|e| e.to_string())
}

/// One confined invocation: the bubblewrap command plus, under the managed
/// route, the host end of the inner's listener handback channel. The
/// bridge's post-spawn half lives in [`super::bridge`].
pub(super) struct WrappedCommand {
    pub(super) command: tokio::process::Command,
    /// `Some` only under the managed route. Consume with
    /// [`super::bridge::BridgeChannel::handoff`] after spawning; dropping
    /// it without a handoff leaves the run without its bridge (the inner
    /// fails the call closed).
    pub(super) bridge: Option<super::bridge::BridgeChannel>,
}

/// Build the confined invocation for `command` in `cwd`: bwrap with the
/// argv above, execing this binary again as the inner helper (sentinel
/// environment), and a `pre_exec` hook that sets `NO_NEW_PRIVS` and — under
/// the managed route — clears `CLOEXEC` on the bridge descriptor so it
/// survives into the sandbox. The hook stays filter-free on purpose:
/// bubblewrap's netns bring-up rides the netlink calls the closed face
/// denies, and the hook body must remain the few direct syscalls that are
/// async-signal-safe between fork and exec. Login-shell PATH and the
/// non-interactive editor/pager env ride along, matching the seatbelt
/// invocation.
pub(super) fn wrap_command(
    roots: &[PathBuf],
    command: &str,
    cwd: &Path,
    route: NetworkRoute,
) -> Result<WrappedCommand, String> {
    let probe = probe().as_ref().ok_or_else(|| {
        "bubblewrap is not available - sandboxed bash cannot run on this host".to_string()
    })?;
    // The inner helper is this process's own executable, re-executed inside
    // the sandbox; its absolute path must resolve or the call fails closed.
    let inner_exe = std::env::current_exe()
        .map_err(|e| format!("the sandbox inner helper needs the host executable path: {e}"))?;
    let (bridge, bridge_fd) = match route {
        NetworkRoute::Managed { .. } => {
            let (channel, inner_fd) = super::bridge::BridgeChannel::pair()
                .map_err(|e| format!("bridge channel creation failed: {e}"))?;
            (Some(channel), Some(inner_fd))
        }
        _ => (None, None),
    };
    let mut cmd = tokio::process::Command::new(&probe.program);
    cmd.args(bwrap_argv(
        roots,
        &inner_exe,
        command,
        probe.supports_as_pid_1,
        probe.supports_preserve_fd,
        route,
        bridge_fd,
    ))
    .current_dir(cwd)
    .env("PATH", super::login_shell_path());
    for (key, value) in super::SandboxPolicy::network_env(route) {
        cmd.env(key, value);
    }
    // The sentinel protocol (`sandbox::inner`): dispatch flag, route face,
    // and — managed only — the bridge descriptor number.
    cmd.env(inner::INNER_ENV, inner::INNER_ENV_VALUE)
        .env(inner::NET_MODE_ENV, NetMode::from_route(route).env_value());
    if let Some(fd) = bridge_fd {
        cmd.env(inner::BRIDGE_FD_ENV, fd.to_string());
    }
    super::inject_noninteractive_env(&mut cmd);
    let clear_fd = bridge_fd;
    // SAFETY: the closure runs in the forked child immediately before exec;
    // prctl and the two fcntls are direct syscalls with no allocation or
    // locking. Clearing CLOEXEC here (in the child's descriptor table, not
    // the parent's) keeps the bridge channel out of unrelated concurrent
    // spawns.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(fd) = clear_fd {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(WrappedCommand {
        command: cmd,
        bridge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_carries_mounts_namespaces_and_the_inner_launch() {
        // Real directories: the argv builder skips missing roots by design,
        // so the bind-order assertions need existing targets.
        let base = tempfile::tempdir().unwrap();
        let ws = base.path().join("ws");
        let extra = base.path().join("extra");
        std::fs::create_dir(&ws).unwrap();
        std::fs::create_dir(&extra).unwrap();
        let exe = Path::new("/opt/manox/bin/steer");
        let argv = bwrap_argv(
            &[ws.clone(), extra.clone()],
            exe,
            "echo hi",
            true,
            false,
            NetworkRoute::Direct,
            None,
        );
        // Base mounts first: read-only root, then the device tree with shm.
        assert_eq!(
            &argv[..8],
            [
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--bind-try",
                "/dev/shm",
                "/dev/shm"
            ]
        );
        assert!(argv.contains(&"--new-session".to_string()));
        assert!(argv.contains(&"--die-with-parent".to_string()));
        assert!(argv.contains(&"--unshare-user".to_string()));
        // Each writable root re-bound read-write over the read-only base,
        // in order, source == destination.
        let bind_positions: Vec<usize> = argv
            .iter()
            .enumerate()
            .filter_map(|(i, a)| (a == "--bind").then_some(i))
            .collect();
        assert_eq!(bind_positions.len(), 2, "{argv:?}");
        for (i, root) in [&ws, &extra].iter().enumerate() {
            let at = bind_positions[i];
            let spelling = root.display().to_string();
            assert_eq!(argv[at + 1], spelling);
            assert_eq!(argv[at + 2], spelling);
        }
        // Pid namespace: bwrap itself is pid 1, with a fresh procfs.
        let pid_at = argv
            .iter()
            .position(|a| a == "--unshare-pid")
            .expect("pid namespace flag present");
        assert_eq!(argv[pid_at + 1], "--as-pid-1");
        assert!(argv.contains(&"--proc".to_string()));
        assert!(argv.contains(&"/proc".to_string()));
        // The direct route shares the host network namespace.
        assert!(!argv.contains(&"--unshare-net".to_string()));
        // The command rides after the inner re-exec: `<exe> -- bash -c`.
        let sep = argv.iter().rposition(|a| a == "--").unwrap();
        assert_eq!(
            &argv[sep + 1..],
            ["bash", "-c", "echo hi"],
            "command is a single argv element"
        );
        // The inner launch sits between bwrap's own `--` and the target.
        let seps: Vec<usize> = argv
            .iter()
            .enumerate()
            .filter_map(|(i, a)| (a == "--").then_some(i))
            .collect();
        assert_eq!(
            seps.len(),
            2,
            "one separator for bwrap, one for the inner: {argv:?}"
        );
        assert_eq!(argv[seps[0] + 1], exe.display().to_string());
        assert_eq!(argv[seps[0] + 2], "--");
    }

    #[test]
    fn argv_without_as_pid_1_drops_the_pid_namespace_flags() {
        let argv = bwrap_argv(
            &[PathBuf::from("/tmp/ws")],
            Path::new("/bin/steer"),
            "echo hi",
            false,
            false,
            NetworkRoute::Direct,
            None,
        );
        assert!(!argv.contains(&"--unshare-pid".to_string()));
        assert!(!argv.contains(&"--as-pid-1".to_string()));
        assert!(!argv.contains(&"--proc".to_string()));
        assert!(argv.contains(&"--unshare-user".to_string()));
    }

    #[test]
    fn argv_skips_missing_writable_roots() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let argv = bwrap_argv(
            &[dir.path().to_path_buf(), missing],
            Path::new("/bin/steer"),
            "true",
            true,
            false,
            NetworkRoute::Direct,
            None,
        );
        let binds: Vec<&[String]> = argv
            .split(|a| a == "--bind")
            .skip(1)
            .map(|rest| &rest[..3.min(rest.len())])
            .collect();
        assert_eq!(binds.len(), 1, "only the existing root binds: {argv:?}");
        assert_eq!(binds[0][1], dir.path().display().to_string());
    }

    #[test]
    fn isolated_routes_unshare_the_network() {
        // The closed route unshares (deny-default by construction); the
        // managed route unshares too — the namespace is its confinement
        // wall, and the bridge is the only door.
        for route in [
            NetworkRoute::Disabled,
            NetworkRoute::Managed { proxy_port: 1 },
        ] {
            let argv = bwrap_argv(
                &[],
                Path::new("/bin/steer"),
                "true",
                true,
                false,
                route,
                None,
            );
            assert!(
                argv.contains(&"--unshare-net".to_string()),
                "{route:?}: {argv:?}"
            );
        }
        // The managed route names the bridge descriptor when the build
        // needs it named.
        let named = bwrap_argv(
            &[],
            Path::new("/bin/steer"),
            "true",
            true,
            true,
            NetworkRoute::Managed { proxy_port: 1 },
            Some(7),
        );
        let at = named.iter().position(|a| a == "--preserve-fd").unwrap();
        assert_eq!(named[at + 1], "7", "{named:?}");
        // The direct route stays on the host net; wrap_command only creates
        // a bridge descriptor under the managed route, so the flag can never
        // ride it.
        let open = bwrap_argv(
            &[],
            Path::new("/bin/steer"),
            "true",
            true,
            true,
            NetworkRoute::Direct,
            None,
        );
        assert!(!open.contains(&"--unshare-net".to_string()), "{open:?}");
        assert!(!open.contains(&"--preserve-fd".to_string()), "{open:?}");
    }

    #[test]
    fn deny_face_covers_exactly_the_escape_primitives() {
        let rules = deny_rules(NetMode::Direct);
        let mut expected = DENIED_SYSCALLS.to_vec();
        expected.sort_unstable();
        let mut actual: Vec<i64> = rules.keys().copied().collect();
        actual.sort_unstable();
        assert_eq!(actual, expected);
        // Every entry is an unconditional match (empty rule chain).
        for (nr, chain) in &rules {
            assert!(chain.is_empty(), "syscall {nr} must deny unconditionally");
        }
        assert!(rules.contains_key(&libc::SYS_ptrace));
        assert!(rules.contains_key(&libc::SYS_process_vm_readv));
        assert!(rules.contains_key(&libc::SYS_process_vm_writev));
        assert!(rules.contains_key(&libc::SYS_io_uring_setup));
        assert!(rules.contains_key(&libc::SYS_io_uring_enter));
        assert!(rules.contains_key(&libc::SYS_io_uring_register));
        // The open face leaves the socket families untouched.
        assert!(!rules.contains_key(&libc::SYS_socket));
        assert!(!rules.contains_key(&libc::SYS_connect));
    }

    #[test]
    fn closed_face_refuses_the_full_socket_lifecycle() {
        let rules = deny_rules(NetMode::Off);
        // Every network-lifecycle syscall denies unconditionally — the
        // single-phase relaxations (sendto, bind, sendmsg, recvmsg, the
        // option calls) are back on the deny face.
        for &nr in NET_DENIED_SYSCALLS {
            assert!(
                rules.get(&nr).is_some_and(|chain| chain.is_empty()),
                "syscall {nr} must deny unconditionally"
            );
        }
        for nr in [
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_recvmsg,
            libc::SYS_bind,
            libc::SYS_getsockopt,
            libc::SYS_setsockopt,
        ] {
            assert!(rules.contains_key(&nr), "syscall {nr} denies again");
        }
        // socket/socketpair carry the AF_UNIX-only condition — netlink
        // included in the denial now that bwrap's bring-up is done.
        for nr in [libc::SYS_socket, libc::SYS_socketpair] {
            let chain = rules.get(&nr).expect("socket family rule present");
            assert_eq!(chain.len(), 1, "syscall {nr}: one conditional rule");
        }
        // The escape primitives ride in every face.
        assert!(rules.contains_key(&libc::SYS_ptrace));
        assert!(rules.contains_key(&libc::SYS_io_uring_setup));
    }

    #[test]
    fn managed_face_gates_socket_families_only() {
        let rules = deny_rules(NetMode::Managed);
        // connect stays open: seccomp cannot see addresses, and the netns
        // already reduced the reachable set to loopback.
        assert!(!rules.contains_key(&libc::SYS_connect));
        assert!(!rules.contains_key(&libc::SYS_sendto));
        // socket denies everything except the bridge hop (inet) and local
        // IPC (unix); socketpair stays unix-only.
        let socket = rules.get(&libc::SYS_socket).expect("socket gate present");
        assert_eq!(socket.len(), 1, "{socket:?}");
        let pair = rules
            .get(&libc::SYS_socketpair)
            .expect("socketpair gate present");
        assert_eq!(pair.len(), 1, "{pair:?}");
        // The escape primitives ride in every face.
        assert!(rules.contains_key(&libc::SYS_ptrace));
        assert!(rules.contains_key(&libc::SYS_io_uring_register));
    }

    #[test]
    fn all_faces_compile_for_the_shipped_targets() {
        for mode in [NetMode::Off, NetMode::Direct, NetMode::Managed] {
            let program = compile_program(mode).expect("the shipped targets compile");
            assert!(!program.is_empty(), "{mode:?}");
        }
    }

    #[test]
    fn userns_denial_keywords_classify() {
        assert!(stderr_namespaces_are_denied(
            "bwrap: setting up uid map: Permission denied"
        ));
        assert!(stderr_namespaces_are_denied(
            "bwrap: Creating new namespace failed: No permissions to create a new namespace"
        ));
        assert!(!stderr_namespaces_are_denied(
            "bwrap: Can't find source path /mnt/missing"
        ));
        assert!(!stderr_namespaces_are_denied(""));
    }

    #[cfg(unix)]
    #[test]
    fn setuid_bit_disqualifies_a_candidate() {
        assert!(has_setuid_bit(0o4755));
        assert!(!has_setuid_bit(0o755));
        assert!(is_executable(0o755));
        assert!(!is_executable(0o644));
    }

    // ---- live tests: real bubblewrap runs (skip loudly without bwrap) ----

    /// The live paths need a passing probe; without it the environment
    /// cannot exercise confinement and the assertions would lie.
    fn live_ready() -> bool {
        if is_available() {
            return true;
        }
        eprintln!("skipping live bwrap test: probe unavailable on this host");
        false
    }

    fn resolver(
        mode: manox_harness::sandbox::AccessMode,
    ) -> std::sync::Arc<dyn Fn() -> manox_harness::sandbox::AccessMode + Send + Sync> {
        std::sync::Arc::new(move || mode)
    }

    fn ops_for(
        root: &Path,
        mode: manox_harness::sandbox::AccessMode,
    ) -> crate::sandbox::SandboxedBashOperations {
        crate::sandbox::SandboxedBashOperations::new(
            root,
            crate::granted_roots::GrantedRoots::new(root),
            resolver(mode),
            std::sync::Arc::new(move || super::NetworkRoute::Direct),
        )
    }

    fn request<'a>(
        command: &'a str,
        cwd: &'a Path,
    ) -> manox_harness::tools::bash::BashExecRequest<'a> {
        manox_harness::tools::bash::BashExecRequest {
            command,
            cwd: Some(cwd),
            timeout: Some(Duration::from_secs(30)),
            signal: tokio_util::sync::CancellationToken::new(),
            on_data: None,
        }
    }

    use manox_harness::tools::bash::BashOperations as _;

    #[tokio::test]
    async fn live_confined_write_admits_the_workspace_and_marks_the_erofs_denial() {
        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for(root.path(), manox_harness::sandbox::AccessMode::Sandbox);

        // A write inside the workspace lands.
        let inside = root.path().join("ok.txt");
        let res = ops
            .exec(request(
                &format!("echo hi > {}", inside.display()),
                root.path(),
            ))
            .await
            .expect("confined write runs");
        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert!(inside.exists(), "file written inside the workspace");

        // A write outside every root hits the read-only base: the EROFS
        // stderr carries the denial marker + escalation hint.
        let res = ops
            .exec(request(
                "touch /usr/local/manox-live-deny-target",
                root.path(),
            ))
            .await
            .expect("exec itself succeeds");
        assert_ne!(res.exit_code, 0, "out-of-bounds write is denied");
        assert!(
            res.stderr
                .to_ascii_lowercase()
                .contains("read-only file system"),
            "EROFS signature in stderr: {}",
            res.stderr
        );
        assert!(
            res.stderr
                .contains("[sandbox: file access denied under sandbox mode]"),
            "denial marker appended: {}",
            res.stderr
        );
        assert!(
            res.stderr.contains("require_escalated"),
            "escalation hint appended: {}",
            res.stderr
        );
        assert!(!Path::new("/usr/local/manox-live-deny-target").exists());
    }

    #[tokio::test]
    async fn live_ptrace_is_denied_by_the_seccomp_filter() {
        if !live_ready() {
            return;
        }
        // python3 calls ptrace(PTRACE_TRACEME) directly: denied -> rc -1 ->
        // exit 0; allowed -> rc 0 -> exit 7 (the failure signal).
        let probe_script =
            "import ctypes,sys;sys.exit(0 if ctypes.CDLL(None).ptrace(0,0,0,0)==-1 else 7)";
        let python3 = std::process::Command::new("python3")
            .arg("-c")
            .arg("pass")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !python3 {
            eprintln!("skipping live ptrace test: python3 unavailable");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for(root.path(), manox_harness::sandbox::AccessMode::Sandbox);
        let res = ops
            .exec(request(
                &format!("python3 -c '{probe_script}'"),
                root.path(),
            ))
            .await
            .expect("sandboxed python runs");
        assert_eq!(
            res.exit_code, 0,
            "ptrace must be EPERM inside the sandbox: {}{}",
            res.stdout, res.stderr
        );
    }

    #[tokio::test]
    async fn live_read_succeeds_and_granted_roots_bind() {
        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for(root.path(), manox_harness::sandbox::AccessMode::Sandbox);
        // Reads are unrestricted (the whole root is ro-bound).
        let res = ops
            .exec(request("ls /dev/null", root.path()))
            .await
            .expect("read runs");
        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    }

    fn ops_for_route(
        root: &Path,
        route: super::NetworkRoute,
    ) -> crate::sandbox::SandboxedBashOperations {
        crate::sandbox::SandboxedBashOperations::new(
            root,
            crate::granted_roots::GrantedRoots::new(root),
            resolver(manox_harness::sandbox::AccessMode::Sandbox),
            std::sync::Arc::new(move || route),
        )
    }

    /// The closed route: no net namespace interface reaches the network, so
    /// curl fails and the stderr carries a network-classified signature —
    /// the marker + escalation hint ride the denial.
    #[tokio::test]
    async fn live_closed_network_denies_curl_and_classifies() {
        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for_route(root.path(), super::NetworkRoute::Disabled);
        let res = ops
            .exec(request("curl -sS -I https://crates.io", root.path()))
            .await
            .expect("exec runs");
        assert_ne!(res.exit_code, 0, "closed network denies curl");
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

    /// The self-check signal reaches the trapped env.
    #[tokio::test]
    async fn live_closed_network_signals_the_self_check_env() {
        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for_route(root.path(), super::NetworkRoute::Disabled);
        let res = ops
            .exec(request(
                "test \"$MANOX_SANDBOX_NETWORK_DISABLED\" = 1",
                root.path(),
            ))
            .await
            .expect("exec runs");
        assert_eq!(
            res.exit_code, 0,
            "self-check env set when closed: {}",
            res.stderr
        );
    }

    /// The tightened closed face: a raw netlink socket — the family the
    /// single-phase shape had to leave open for bwrap's bring-up — is
    /// EPERM inside the sandbox now that the filter installs after it.
    #[tokio::test]
    async fn live_closed_face_denies_netlink_sockets() {
        if !live_ready() {
            return;
        }
        let python3 = std::process::Command::new("python3")
            .arg("-c")
            .arg("pass")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !python3 {
            eprintln!("skipping live netlink test: python3 unavailable");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for_route(root.path(), super::NetworkRoute::Disabled);
        // socket(AF_NETLINK) → EPERM → the probe exits 0; a pass-through
        // would create the socket and exit 7.
        let script = "import socket,sys\n\
                      try:\n    socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 0)\n\
                      except PermissionError:\n    sys.exit(0)\n\
                      except OSError as e:\n    sys.exit(0 if e.errno == 1 else 7)\n\
                      sys.exit(7)";
        let res = ops
            .exec(request(&format!("python3 -c '{script}'"), root.path()))
            .await
            .expect("exec runs");
        assert_eq!(
            res.exit_code, 0,
            "netlink sockets must be EPERM under the closed face: {}{}",
            res.stdout, res.stderr
        );
    }

    /// The direct route shares the host net (through the inner helper now):
    /// curl reaches out with no proxy env in the way.
    #[tokio::test]
    async fn live_open_network_reaches_the_network_directly() {
        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for_route(root.path(), super::NetworkRoute::Direct);
        let res = ops
            .exec(request("curl -sS -I https://crates.io", root.path()))
            .await
            .expect("exec runs");
        assert_eq!(
            res.exit_code, 0,
            "the direct route reaches the network: {}",
            res.stderr
        );
    }

    /// The managed route end to end: an allowlisted host tunnels through
    /// the netns bridge and the session proxy; a miss is refused at the
    /// judge (no approver installed).
    #[tokio::test(flavor = "multi_thread")]
    async fn live_managed_route_tunnels_allowlisted_and_refuses_misses() {
        use crate::sandbox::netpolicy::NetworkPolicy;
        use manox_harness::sandbox::NetworkAccess;

        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let policy = NetworkPolicy::for_allowlist(vec!["crates.io".to_string()]);
        policy.set_enabled(NetworkAccess::On).await;
        let route = policy.route_for(NetworkAccess::On);
        let NetworkRoute::Managed { proxy_port: _ } = route else {
            panic!("an allowlisted policy binds the managed route, got {route:?}");
        };
        let policy_for_resolver = policy.clone();
        let ops = crate::sandbox::SandboxedBashOperations::new(
            root.path(),
            crate::granted_roots::GrantedRoots::new(root.path()),
            resolver(manox_harness::sandbox::AccessMode::Sandbox),
            std::sync::Arc::new(move || policy_for_resolver.route_for(NetworkAccess::On)),
        );

        // Allowlisted: the tunnel carries it through the bridge.
        let res = ops
            .exec(request("curl -sS -I https://crates.io", root.path()))
            .await
            .expect("exec runs");
        assert_eq!(
            res.exit_code, 0,
            "allowlisted host tunnels through the managed bridge: {} / {}",
            res.stdout, res.stderr
        );

        // A miss: the host judge refuses (no approver installed) and curl
        // sees the tunnel refusal.
        let res = ops
            .exec(request("curl -sS -I https://example.com", root.path()))
            .await
            .expect("exec runs");
        assert_ne!(res.exit_code, 0, "a non-allowlisted host is refused");
    }

    /// The managed face keeps local servers usable: a loopback listener
    /// inside the netns binds and serves (bind/listen stay open there).
    #[tokio::test]
    async fn live_managed_route_admits_loopback_servers() {
        use crate::sandbox::netpolicy::NetworkPolicy;
        use manox_harness::sandbox::NetworkAccess;

        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let policy = NetworkPolicy::for_allowlist(vec!["crates.io".to_string()]);
        policy.set_enabled(NetworkAccess::On).await;
        let policy_for_resolver = policy.clone();
        let ops = crate::sandbox::SandboxedBashOperations::new(
            root.path(),
            crate::granted_roots::GrantedRoots::new(root.path()),
            resolver(manox_harness::sandbox::AccessMode::Sandbox),
            std::sync::Arc::new(move || policy_for_resolver.route_for(NetworkAccess::On)),
        );
        // python binds loopback, answers one request, and the client (same
        // netns) reads it back: bind/listen/connect/accept all ride the
        // managed face.
        let script = "import socket,threading\n\
                      s=socket.socket();s.bind((\"127.0.0.1\",0));s.listen(1)\n\
                      port=s.getsockname()[1]\n\
                      def serve():\n    c,_=s.accept();c.sendall(b\"pong\");c.close()\n\
                      t=threading.Thread(target=serve);t.start()\n\
                      c=socket.socket();c.connect((\"127.0.0.1\",port))\n\
                      assert c.recv(4)==b\"pong\"\n\
                      t.join();s.close()";
        let res = ops
            .exec(request(&format!("python3 -c '{script}'"), root.path()))
            .await
            .expect("exec runs");
        assert_eq!(
            res.exit_code, 0,
            "loopback servers stay usable under the managed face: {}{}",
            res.stdout, res.stderr
        );
    }

    /// A sandboxed run's env carries no sentinel trace: the inner strips
    /// the protocol variables before the target execs.
    #[tokio::test]
    async fn live_target_env_carries_no_sentinel() {
        if !live_ready() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let ops = ops_for_route(root.path(), super::NetworkRoute::Disabled);
        let res = ops
            .exec(request("test -z \"$MANOX_SANDBOX_INNER\"", root.path()))
            .await
            .expect("exec runs");
        assert_eq!(
            res.exit_code, 0,
            "sentinel stripped before the target: {}",
            res.stderr
        );
    }
}
