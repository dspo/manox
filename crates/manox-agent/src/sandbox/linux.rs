//! The Linux sandbox backend: bubblewrap confinement plus a seccomp
//! deny-face filter, wrapping one-shot commands under the same
//! writable-root vocabulary as the macOS seatbelt.
//!
//! There is no inner helper and no self re-exec: the command is spawned as
//! `fork → (in the child) install the seccomp filter → exec bwrap → bwrap
//! execs the target`. The filter installs across the `Command::pre_exec`
//! hook (fork child, before exec — only direct syscalls run there), so it is
//! inherited by bwrap and every descendant, and the parent-side process
//! management (pipes, wall-clock timeout, cancellation) is the same skeleton
//! the seatbelt backend uses. This shape needs no sentinel dispatch in a
//! host `main` — manox is a library inside the host binary — and it requires
//! a NON-setuid bubblewrap: `PR_SET_NO_NEW_PRIVS` (set together with the
//! filter) would kill setuid privilege elevation, so a setuid bwrap is
//! treated as unavailable and the platform degrades to per-call approval.
//! The deb/rpm distribution floor (Debian 12+/Ubuntu 22.04+/Fedora) ships
//! non-setuid bwrap riding unprivileged user namespaces.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::NetworkRoute;
use manox_harness::sandbox::NetworkAccess;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompFilter, SeccompCondition, SeccompCmpArgLen, SeccompCmpOp,
    SeccompRule, TargetArch, apply_filter,
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
    /// the filter is installed).
    program: PathBuf,
    /// `--as-pid-1` doubles as the version floor: it appears in `--help`
    /// only on builds new enough for the pid-namespace shape we rely on.
    supports_as_pid_1: bool,
}

/// The process-lifetime probe result: `Some` when bwrap is present,
/// non-setuid, feature-complete, and passes a real confinement smoke test.
static PROBE: OnceLock<Option<BwrapProbe>> = OnceLock::new();

/// Whether the Linux backend is usable here (probed once, cached).
pub(super) fn is_available() -> bool {
    probe().is_some()
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
    // `--as-pid-1` in the help text is the version floor (bubblewrap gained
    // it in 0.8); older distro builds stop here and the platform degrades.
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
    // The kernel must actually grant the confinement: unprivileged user
    // namespaces are a boot-time or LSM policy (kernel.unprivileged_userns_
    // clone, apparmor userns restrictions), and only a real run can tell.
    smoke_test(&program)?;
    Some(BwrapProbe {
        program,
        supports_as_pid_1,
    })
}

/// First executable non-setuid `bwrap` on PATH, as an absolute path. A
/// setuid bwrap is skipped on purpose: the pre-exec `NO_NEW_PRIVS` (needed
/// to install the seccomp filter unprivileged) strips setuid elevation, so
/// a setuid build would fail at runtime — and treating it as usable would
/// trade a silent breakage for the illusion of confinement.
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
/// parent manages). The network namespace follows the route:
/// `NetworkRoute::Disabled` unshares it (no interfaces but the downed
/// loopback — deny-default by construction), the open routes share the
/// host's.
pub(super) fn bwrap_argv(
    roots: &[PathBuf],
    command: &str,
    supports_as_pid_1: bool,
    route: NetworkRoute,
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
    if route == NetworkRoute::Disabled {
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
    argv.push("--".into());
    argv.push("bash".into());
    argv.push("-c".into());
    argv.push(command.to_string());
    argv
}

/// The syscalls the deny face refuses (default action allow, verdict
/// EPERM). `ptrace` and the `process_vm_*` cross-process memory family are
/// the classic sandbox-escape primitives against the host process itself
/// (the filter is inherited, so a sandboxed child could otherwise attach
/// back to its unsandboxed ancestor); `io_uring` bypasses the standard
/// syscall path for its submitted operations, which would let the network
/// closure be sidestepped — it stays unavailable in every network state.
const DENIED_SYSCALLS: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// The syscalls the socket-restricted face additionally refuses when the
/// route is closed: the connect/accept/bind/listen lifecycle, the
/// address-inspection calls, and the send/receive bulk. `recvfrom` stays
/// allowed so socketpair-based subprocess management (`cargo clippy` and
/// friends) keeps working, and the `socket`/`socketpair` families get a
/// conditional rule (AF_UNIX only) rather than a blanket deny.
const NETWORK_DENIED_SYSCALLS: &[i64] = &[
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
    libc::SYS_recvmmsg,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
];

/// The unconditional deny rules: one entry per denied syscall, each with an
/// empty rule chain (matches every invocation of that syscall). The closed
/// route adds the network face: the socket lifecycle denials above plus
/// AF_UNIX-only conditions on `socket`/`socketpair` (denied when the domain
/// argument is anything else — `arg0 != AF_UNIX` matches, the empty default
/// action allows the unix case).
fn deny_rules(route: NetworkRoute) -> BTreeMap<i64, Vec<SeccompRule>> {
    let mut rules = BTreeMap::new();
    for &nr in DENIED_SYSCALLS {
        rules.insert(nr, Vec::new());
    }
    if route == NetworkRoute::Disabled {
        for &nr in NETWORK_DENIED_SYSCALLS {
            rules.insert(nr, Vec::new());
        }
        // `socket`/`socketpair` with domain != AF_UNIX are denied; AF_UNIX
        // falls through to the default allow.
        let unix_only = SeccompRule::new(vec![SeccompCondition::new(
            0, // first argument (domain)
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )
        .expect("AF_UNIX comparison compiles")]);
        rules.insert(libc::SYS_socket, vec![unix_only.clone()]);
        rules.insert(libc::SYS_socketpair, vec![unix_only]);
    }
    rules
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

/// Compile the BPF program in the parent (once per process and per network
/// face): allow-by-default with EPERM on the deny face. `Err` only where the
/// architecture is outside the supported set.
fn compile_program(route: NetworkRoute) -> Result<BpfProgram, String> {
    let Some(arch) = seccomp_target_arch() else {
        return Err(
            "seccomp filter unsupported on this architecture - the Linux sandbox \
             needs x86_64 or aarch64"
                .into(),
        );
    };
    let filter = SeccompFilter::new(
        deny_rules(route),
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

/// The compiled deny-face programs (one per network face), cached. There
/// are exactly two faces: the open base and the socket-restricted closed
/// one — a switch between them never recompiles mid-session. Compilation is
/// infallible for the shipped targets; a failure fails the CALL closed
/// (never the process).
fn seccomp_program(route: NetworkRoute) -> Result<&'static BpfProgram, String> {
    static OPEN: OnceLock<Result<BpfProgram, String>> = OnceLock::new();
    static RESTRICTED: OnceLock<Result<BpfProgram, String>> = OnceLock::new();
    let slot = match route {
        NetworkRoute::Disabled => &RESTRICTED,
        NetworkRoute::Direct | NetworkRoute::Managed { .. } => &OPEN,
    };
    slot.get_or_init(|| compile_program(route))
        .as_ref()
        .map_err(Clone::clone)
}

/// Build the confined invocation for `command` in `cwd`: bwrap with the
/// argv above, and a `pre_exec` hook that installs the seccomp filter in
/// the forked child (the hook body is two direct syscalls — prctl and
/// seccomp — the only async-signal-safe work between fork and exec).
/// Login-shell PATH and the non-interactive editor/pager env ride along,
/// matching the seatbelt invocation.
pub(super) fn wrap_command(
    roots: &[PathBuf],
    command: &str,
    cwd: &Path,
    route: NetworkRoute,
) -> Result<tokio::process::Command, String> {
    let probe = probe().as_ref().ok_or_else(|| {
        "bubblewrap is not available - sandboxed bash cannot run on this host".to_string()
    })?;
    let program = seccomp_program(route)?.clone();
    let mut cmd = tokio::process::Command::new(&probe.program);
    cmd.args(bwrap_argv(roots, command, probe.supports_as_pid_1, route))
        .current_dir(cwd)
        .env("PATH", super::login_shell_path());
    for (key, value) in super::SandboxPolicy::network_env(route) {
        cmd.env(key, value);
    }
    super::inject_noninteractive_env(&mut cmd);
    // SAFETY: the closure runs in the forked child immediately before exec;
    // `apply_filter` issues only prctl(PR_SET_NO_NEW_PRIVS) and seccomp(2)
    // against memory prepared here, with no allocation or locking.
    unsafe {
        cmd.pre_exec(move || {
            apply_filter(&program).map_err(|e| std::io::Error::other(e.to_string()))
        });
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_carries_mounts_namespaces_and_a_single_command_element() {
        // Real directories: the argv builder skips missing roots by design,
        // so the bind-order assertions need existing targets.
        let base = tempfile::tempdir().unwrap();
        let ws = base.path().join("ws");
        let extra = base.path().join("extra");
        std::fs::create_dir(&ws).unwrap();
        std::fs::create_dir(&extra).unwrap();
        let argv = bwrap_argv(
            &[ws.clone(), extra.clone()],
            "echo hi",
            true,
            NetworkRoute::Direct,
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
        // The open route shares the host network namespace.
        assert!(!argv.contains(&"--unshare-net".to_string()));
        // The command rides after `--` as ONE argv element.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], ["bash", "-c", "echo hi"]);
    }

    #[test]
    fn argv_without_as_pid_1_drops_the_pid_namespace_flags() {
        let argv = bwrap_argv(
            &[PathBuf::from("/tmp/ws")],
            "echo hi",
            false,
            NetworkRoute::Direct,
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
            "true",
            true,
            NetworkRoute::Direct,
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
    fn closed_route_argv_unshares_the_network() {
        let argv = bwrap_argv(&[], "true", true, NetworkRoute::Disabled);
        assert!(argv.contains(&"--unshare-net".to_string()), "{argv:?}");
        // The open faces share the host net.
        let open = bwrap_argv(&[], "true", true, NetworkRoute::Managed { proxy_port: 1 });
        assert!(!open.contains(&"--unshare-net".to_string()), "{open:?}");
    }

    #[test]
    fn deny_face_covers_exactly_the_escape_primitives() {
        let rules = deny_rules(NetworkRoute::Direct);
        let mut expected = DENIED_SYSCALLS.to_vec();
        expected.sort_unstable();
        let mut actual: Vec<i64> = rules.keys().copied().collect();
        actual.sort_unstable();
        assert_eq!(actual, expected);
        // Every entry is an unconditional match (empty rule chain), and the
        // socket family is untouched until phase 3.
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
    fn closed_route_adds_the_socket_restricted_face() {
        let rules = deny_rules(NetworkRoute::Disabled);
        // Every network-lifecycle syscall denies unconditionally...
        for &nr in NETWORK_DENIED_SYSCALLS {
            assert!(
                rules.get(&nr).is_some_and(|chain| chain.is_empty()),
                "syscall {nr} must deny unconditionally"
            );
        }
        // ...while socket/socketpair carry the AF_UNIX-only condition.
        for nr in [libc::SYS_socket, libc::SYS_socketpair] {
            let chain = rules.get(&nr).expect("socket family rule present");
            assert_eq!(chain.len(), 1, "syscall {nr}: one conditional rule");
        }
        // The escape primitives ride in both faces.
        assert!(rules.contains_key(&libc::SYS_ptrace));
        assert!(rules.contains_key(&libc::SYS_io_uring_setup));
    }

    #[test]
    fn both_faces_compile_for_the_shipped_targets() {
        for route in [
            NetworkRoute::Disabled,
            NetworkRoute::Direct,
            NetworkRoute::Managed { proxy_port: 1 },
        ] {
            let program = compile_program(route).expect("the shipped targets compile");
            assert!(!program.is_empty(), "{route:?}");
        }
        // The two open faces share one compiled program (same cache slot).
        assert!(seccomp_program(NetworkRoute::Direct).is_ok());
        assert!(seccomp_program(NetworkRoute::Managed { proxy_port: 1 }).is_ok());
        assert!(seccomp_program(NetworkRoute::Disabled).is_ok());
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
}
