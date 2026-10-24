//! The inner sandbox helper: the host binary re-executing itself inside the
//! sandbox as its own trampoline, dispatched by a constructor.
//!
//! manox is a library inside a host binary (the desktop app, a test binary),
//! so there is no dedicated helper executable — the helper is the binary
//! itself, spawned a second time under bubblewrap with a sentinel
//! environment. The sentinel protocol (all variables are stripped before the
//! target command execs, so they never reach user code):
//!
//! - `MANOX_SANDBOX_INNER=1` — the dispatch flag. A `.init_array` static in
//!   this module scans `environ` for it at process start; on a hit it calls
//!   [`inner_main`], which never returns. This is the only self-dispatch
//!   channel a library can install without the host `main`'s cooperation.
//!   The worst shape of a stray sentinel in the host environment is the
//!   process starting up and exiting immediately — equivalent to a bad CLI
//!   argument — because the constructor runs before any runtime state is
//!   touched and only exits.
//! - `MANOX_SANDBOX_INNER_NET` — the route face: `off` (closed seccomp
//!   face), `direct` (escape-primitives face only), `managed` (bridge plus
//!   the loopback-gated face).
//! - `MANOX_SANDBOX_INNER_BRIDGE_FD` — the control-channel descriptor
//!   number, present only under `managed`. The host created the socketpair
//!   before the spawn and cleared `CLOEXEC` on this end in its `pre_exec`
//!   hook, so the descriptor survives the exec into bubblewrap and into this
//!   process (bubblewrap passes inherited descriptors through; verified
//!   against 0.9.0).
//!
//! The inner runs inside the namespaces bubblewrap built (network, user,
//! pid). Ordinary Rust rules apply — no async-signal-safety constraints,
//! allocation and threads are fine. Its jobs, in order:
//!
//! 1. Validate the sentinel environment (the descriptors exist, the argv
//!    tail has the expected shape) — a mistyped sentinel fails closed with
//!    exit code 126 instead of conferring any access.
//! 2. Under `managed`: bind the netns loopback listener, send it to the
//!    host over the control channel (`SCM_RIGHTS`), and wait for the host's
//!    ready acknowledgment; then rewrite the `*_PROXY` variables the host
//!    injected to point at the netns-side listener port.
//! 3. Install the seccomp face. The filter lives here — not in the host's
//!    `pre_exec` — because bubblewrap's own netns bring-up rides the
//!    netlink syscalls the closed face denies; after bwrap has finished,
//!    they can go back on the deny list (this tightens the single-phase
//!    face that had to leave `bind`/`sendto`/`sendmsg`/`recvmsg` and the
//!    netlink socket family allowed).
//! 4. Strip the sentinel variables, fork, and exec the target command,
//!    forwarding SIGHUP/INT/QUIT/TERM to it and exiting with its exact wait
//!    status. The target only ever runs if the inner completed every step;
//!    a broken helper fails the call closed.

use std::io::Read;
use std::net::TcpListener;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::time::Duration;

use super::NetworkRoute;
use super::linux;

/// The dispatch sentinel: `=1` in the inner's environment only.
pub(super) const INNER_ENV: &str = "MANOX_SANDBOX_INNER";
pub(super) const INNER_ENV_VALUE: &str = "1";
/// The route face the inner runs under (values in [`NetMode`]).
pub(super) const NET_MODE_ENV: &str = "MANOX_SANDBOX_INNER_NET";
/// The control-channel descriptor number (`managed` only).
pub(super) const BRIDGE_FD_ENV: &str = "MANOX_SANDBOX_INNER_BRIDGE_FD";

/// Control-channel bytes: the inner sends the listener marked with this,
/// the host answers readiness with [`BRIDGE_READY`].
const BRIDGE_LISTENER: u8 = 1;
const BRIDGE_READY: u8 = 1;

/// How long either side waits for the handoff before failing the call.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

/// Signals the inner forwards to the target command while it waits.
const FORWARDED_SIGNALS: [libc::c_int; 4] =
    [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// The proxied env keys the host injects for the managed route (the same
/// set [`super::SandboxPolicy::network_env`] writes). The inner rewrites
/// each to the netns-side listener port.
const PROXY_ENV_KEYS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// The route face one inner run enforces, mirroring [`NetworkRoute`] minus
/// the proxy port (the host keeps that; the inner only needs the face).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NetMode {
    /// `networkAccess=off`: the closed face, no bridge.
    Off,
    /// `networkAccess=on`, no allowlist: shared host network, escape
    /// primitives only.
    Direct,
    /// `networkAccess=on` with an allowlist: isolated netns plus the
    /// loopback bridge.
    Managed,
}

impl NetMode {
    pub(super) fn from_route(route: NetworkRoute) -> Self {
        match route {
            NetworkRoute::Disabled => Self::Off,
            NetworkRoute::Direct => Self::Direct,
            NetworkRoute::Managed { .. } => Self::Managed,
        }
    }

    pub(super) fn env_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Direct => "direct",
            Self::Managed => "managed",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "off" => Some(Self::Off),
            "direct" => Some(Self::Direct),
            "managed" => Some(Self::Managed),
            _ => None,
        }
    }
}

unsafe extern "C" {
    /// glibc's process-global environment vector; the libc crate does not
    /// re-export it on linux-gnu, so the constructor declares it here.
    static environ: *mut *mut libc::c_char;
}

/// The constructor dispatch: scan `environ` at process start for the
/// sentinel and, on a hit, become the inner helper. Everything here is
/// libc-level until the sentinel verdict is in — no allocation, no runtime
/// state — so a normal host process pays one environ walk.
#[used]
#[unsafe(link_section = ".init_array")]
static SANDBOX_INNER_DISPATCH: extern "C" fn() = sandbox_inner_ctor;

extern "C" fn sandbox_inner_ctor() {
    // SAFETY: `environ` is the process-global vector glibc maintains; the
    // scan only reads NUL-terminated entries and never mutates anything.
    let env = unsafe { environ };
    if !env.is_null() && environ_holds_sentinel(env) {
        inner_main();
    }
}

/// Whether the raw environment holds `MANOX_SANDBOX_INNER=1` as a whole
/// entry. Byte-wise comparison — the constructor runs before the Rust
/// runtime, so this allocates nothing.
fn environ_holds_sentinel(env: *mut *mut libc::c_char) -> bool {
    // concat! needs literals, so the needle repeats the constants verbatim;
    // `protocol_env_names_are_stable` guards the cross-exec contract.
    const NEEDLE: &[u8] = b"MANOX_SANDBOX_INNER=1";
    // SAFETY: `environ` is a NULL-terminated array of NUL-terminated
    // strings (see the constructor above).
    unsafe {
        let mut cursor = env;
        while !(*cursor).is_null() {
            let mut entry = *cursor;
            let mut needle = NEEDLE.iter();
            let mut matched = true;
            for &expected in needle {
                if *entry != expected as libc::c_char {
                    matched = false;
                    break;
                }
                entry = entry.add(1);
            }
            if matched && *entry == 0 {
                return true;
            }
            cursor = cursor.add(1);
        }
    }
    false
}

/// The inner helper's whole life. Never returns: every path either execs
/// into the target command (through the forked child's wait) or exits.
fn inner_main() -> ! {
    let exit = |message: &str| -> ! {
        eprintln!("manox sandbox inner helper: {message}");
        std::process::exit(126);
    };
    let mode = match std::env::var(NET_MODE_ENV).as_deref() {
        Ok(raw) => match NetMode::parse(raw) {
            Some(mode) => mode,
            None => exit(&format!("unknown {NET_MODE_ENV} value {raw:?}")),
        },
        Err(_) => exit(&format!("{NET_MODE_ENV} missing")),
    };

    // Managed first: the bridge must exist before the seccomp face lands —
    // binding the listener and the SCM_RIGHTS send ride syscalls the face
    // denies (bind, sendmsg, setsockopt).
    let netns_port = if mode == NetMode::Managed {
        let raw_fd = match std::env::var(BRIDGE_FD_ENV) {
            Ok(raw) => match raw.parse::<RawFd>() {
                Ok(fd) if fd > 2 => fd,
                _ => exit(&format!("invalid {BRIDGE_FD_ENV} {raw:?}")),
            },
            Err(_) => exit(&format!("{BRIDGE_FD_ENV} missing")),
        };
        // The descriptor must actually be live: the sentinel without its
        // channel is a protocol violation, not a bypass.
        // SAFETY: F_GETFD reads flags of a descriptor number; EBADF is the
        // expected signal for a stale number.
        if unsafe { libc::fcntl(raw_fd, libc::F_GETFD) } < 0 {
            exit(&format!("{BRIDGE_FD_ENV} {raw_fd} is not open"));
        }
        // SAFETY: the descriptor was inherited across exec and its Rust
        // owner lives in the host; this process owns it from here on.
        let mut control = unsafe { UnixStream::from_raw_fd(raw_fd) };
        // The channel end must not leak into the target command.
        // SAFETY: F_SETFD writes flag bits on an owned descriptor.
        unsafe {
            let flags = libc::fcntl(control.as_raw_fd(), libc::F_GETFD);
            let _ = libc::fcntl(control.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
        let _ = control.set_read_timeout(Some(HANDOFF_TIMEOUT));
        let _ = control.set_write_timeout(Some(HANDOFF_TIMEOUT));
        let listener = match bind_loopback_listener() {
            Ok(listener) => listener,
            Err(error) => exit(&format!("netns loopback bind failed: {error}")),
        };
        let netns_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        if let Err(error) = send_listener(&control, &listener) {
            exit(&format!("listener handoff failed: {error}"));
        }
        drop(listener);
        let mut ready = [0u8; 1];
        match control.read_exact(&mut ready) {
            Ok(_) if ready == [BRIDGE_READY] => {}
            _ => exit("host bridge did not accept the listener"),
        }
        // The control end stays open for exactly this run: the host watches
        // it for EOF to tear the relay down when the inner exits.
        std::mem::forget(control);
        netns_port
    } else {
        0
    };

    if mode == NetMode::Managed {
        rewrite_proxy_env(netns_port);
    }
    if let Err(error) = linux::install_seccomp(mode) {
        exit(&format!("seccomp face failed to install: {error}"));
    }
    clear_sentinels();

    // The target argv rides after our own `--` separator (bwrap execed us
    // as `<exe> -- bash -c <command>`); anything else is a protocol breach.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let target = match args.as_slice() {
        [sep, rest @ ..] if sep == "--" && !rest.is_empty() => rest.to_vec(),
        _ => exit("inner argv is missing the target command"),
    };
    run_target(target)
}

/// Bind an IPv4 loopback listener on an ephemeral port. bubblewrap raises
/// `lo` when it creates the network namespace, so the first bind is
/// expected to succeed; the failure message carries the errno for the log.
fn bind_loopback_listener() -> std::io::Result<TcpListener> {
    TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
}

/// Send the netns listener to the host over the control channel: one
/// marker byte plus the descriptor in a `SCM_RIGHTS` ancillary record.
pub(super) fn send_listener(control: &UnixStream, listener: &TcpListener) -> std::io::Result<()> {
    use std::io::IoSlice;
    let descriptors = [listener.as_fd()];
    let mut space = [0u8; rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = rustix::net::SendAncillaryBuffer::new(&mut space);
    if !ancillary.push(rustix::net::SendAncillaryMessage::ScmRights(&descriptors)) {
        return Err(std::io::Error::other("listener control header did not fit"));
    }
    let sent = rustix::io::retry_on_intr(|| {
        rustix::net::sendmsg(
            control,
            &[IoSlice::new(&[BRIDGE_LISTENER])],
            &mut ancillary,
            rustix::net::SendFlags::empty(),
        )
    })?;
    if sent != 1 {
        return Err(std::io::Error::other("listener handoff wrote no bytes"));
    }
    Ok(())
}

/// Whether `value` is a loopback proxy URL of the shape the host injects
/// (`http://127.0.0.1:<port>`).
fn is_loopback_proxy_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Point the injected `*_PROXY` variables at the netns-side listener. The
/// host wrote the host-side proxy port; inside the namespace only the
/// netns loopback is reachable, so the port must be this run's listener.
fn rewrite_proxy_env(netns_port: u16) {
    let url = format!("http://127.0.0.1:{netns_port}");
    for key in PROXY_ENV_KEYS {
        if let Ok(value) = std::env::var(key)
            && is_loopback_proxy_url(&value)
        {
            // SAFETY: the inner is single-threaded here — the constructor
            // ran before any thread could have been spawned, and the target
            // command has not execed yet.
            unsafe { std::env::set_var(key, &url) };
        }
    }
}

/// Strip the sentinel variables so the target command's environment carries
/// no trace of the inner protocol.
fn clear_sentinels() {
    // SAFETY: single-threaded constructor context; see `rewrite_proxy_env`.
    unsafe {
        std::env::remove_var(INNER_ENV);
        std::env::remove_var(NET_MODE_ENV);
        std::env::remove_var(BRIDGE_FD_ENV);
    }
}

/// The direct child's pid and the latest signal, read by the async-signal-
/// safe forwarder. Written before the handlers are installed and before the
/// blocked window opens, so a handler never sees a stale pid.
static TARGET_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_signal(signal: libc::c_int) {
    let child = TARGET_PID.load(std::sync::atomic::Ordering::SeqCst);
    if child > 0 {
        // SAFETY: kill of a recorded pid; the handler's whole body.
        unsafe { libc::kill(child, signal) };
    }
}

/// Fork, exec the target in the child, and exit the parent with the
/// target's exact wait status. The forwarded signals are blocked around the
/// fork so none lands between fork and handler installation (blocked
/// deliveries stay pending and fire into the installed handlers on
/// unblock); the parent's handlers kill the child, and once the child is
/// reaped the parent re-raises its own death signal (or exits with the
/// code) so the host sees the same outcome it would have seen from the
/// command directly.
fn run_target(target: Vec<std::ffi::OsString>) -> ! {
    // SAFETY: each block only calls signal syscalls on stack-local sets.
    unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        for signal in FORWARDED_SIGNALS {
            libc::sigaddset(&mut blocked, signal);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous) != 0 {
            eprintln!(
                "manox sandbox inner helper: failed to block signals: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(126);
        }
        let pid = libc::fork();
        if pid < 0 {
            eprintln!(
                "manox sandbox inner helper: fork failed: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(126);
        }
        if pid == 0 {
            // Child: default dispositions, unblocked signals, exec. On an
            // exec failure the message must reach the call's stderr.
            for signal in FORWARDED_SIGNALS {
                libc::signal(signal, libc::SIG_DFL);
            }
            libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
            let mut command = std::process::Command::new(&target[0]);
            command.args(&target[1..]);
            let error = command.exec();
            eprintln!("manox sandbox inner helper: exec failed: {error}");
            std::process::exit(127);
        }
        TARGET_PID.store(pid, std::sync::atomic::Ordering::SeqCst);
        for signal in FORWARDED_SIGNALS {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                eprintln!(
                    "manox sandbox inner helper: signal handler failed: {}",
                    std::io::Error::last_os_error()
                );
                std::process::exit(126);
            }
        }
        // Unblock: signals that arrived while blocked now deliver into the
        // installed handlers (a pending signal is not lost).
        libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        loop {
            let mut status: libc::c_int = 0;
            let reaped = libc::waitpid(-1, &mut status, 0);
            if reaped == pid {
                exit_with_wait_status(status);
            }
            if reaped >= 0 {
                continue; // an orphaned grandchild; not ours to translate
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!(
                "manox sandbox inner helper: waitpid failed: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(126);
        }
    }
}

/// Translate a waitpid status into this process's own death: the exit code
/// as-is, a death signal re-raised with the default handler (so the host's
/// wait sees `WIFSIGNALED`), anything else a generic failure.
fn exit_with_wait_status(status: libc::c_int) -> ! {
    if libc::WIFEXITED(status) {
        std::process::exit(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        // SAFETY: reset-then-raise reproduces the child's death signal for
        // the host's wait.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::kill(libc::getpid(), signal);
        }
        std::process::exit(128 + signal);
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic environ array from string entries (NULL-terminated, the
    /// shape glibc's `environ` has), kept alive by the returned Vec.
    fn synthetic_environ(entries: &[&str]) -> (Vec<std::ffi::CString>, Vec<*mut libc::c_char>) {
        let cstrings: Vec<std::ffi::CString> = entries
            .iter()
            .map(|e| std::ffi::CString::new(*e).unwrap())
            .collect();
        let mut pointers: Vec<*mut libc::c_char> = cstrings
            .iter()
            .map(|c| c.as_ptr() as *mut libc::c_char)
            .collect();
        pointers.push(std::ptr::null_mut());
        (cstrings, pointers)
    }

    #[test]
    fn the_sentinel_scan_hits_only_the_exact_entry() {
        let (keep, mut env) = synthetic_environ(&[
            "PATH=/usr/bin",
            "MANOX_SANDBOX_INNER=1",
            "MANOX_SANDBOX_INNER_NET=managed",
        ]);
        assert!(environ_holds_sentinel(env.as_mut_ptr()), "{keep:?}");
        let (keep, mut env) =
            synthetic_environ(&["PATH=/usr/bin", "MANOX_SANDBOX_INNER_NET=managed"]);
        assert!(!environ_holds_sentinel(env.as_mut_ptr()), "{keep:?}");
        // A value other than `1` is not the dispatch flag.
        let (keep, mut env) = synthetic_environ(&["MANOX_SANDBOX_INNER=0"]);
        assert!(!environ_holds_sentinel(env.as_mut_ptr()), "{keep:?}");
        // A prefix-shaped neighbor never matches.
        let (keep, mut env) = synthetic_environ(&["MANOX_SANDBOX_INNER=12"]);
        assert!(!environ_holds_sentinel(env.as_mut_ptr()), "{keep:?}");
    }

    #[test]
    fn an_empty_environ_has_no_sentinel() {
        let (_, mut env) = synthetic_environ(&[]);
        assert!(!environ_holds_sentinel(env.as_mut_ptr()));
    }

    #[test]
    fn proxy_url_recognition_matches_the_injected_shape() {
        assert!(is_loopback_proxy_url("http://127.0.0.1:54321"));
        assert!(is_loopback_proxy_url("http://127.0.0.1:0"));
        assert!(!is_loopback_proxy_url("http://127.0.0.1:"));
        assert!(!is_loopback_proxy_url("http://127.0.0.1:80x"));
        assert!(!is_loopback_proxy_url("https://127.0.0.1:54321"));
        assert!(!is_loopback_proxy_url("http://localhost:54321"));
        assert!(!is_loopback_proxy_url("http://proxy.internal:8080"));
    }

    #[test]
    fn net_mode_round_trips_through_the_env_vocabulary() {
        for (route, expected) in [
            (NetworkRoute::Disabled, NetMode::Off),
            (NetworkRoute::Direct, NetMode::Direct),
            (NetworkRoute::Managed { proxy_port: 1 }, NetMode::Managed),
        ] {
            let mode = NetMode::from_route(route);
            assert_eq!(mode, expected, "{route:?}");
            assert_eq!(NetMode::parse(mode.env_value()), Some(mode), "{route:?}");
        }
        assert_eq!(NetMode::parse("bogus"), None);
    }

    #[test]
    fn protocol_env_names_are_stable() {
        // The host (linux.rs) and the inner share these names across an
        // exec boundary; a rename here is a protocol break.
        assert_eq!(INNER_ENV, "MANOX_SANDBOX_INNER");
        assert_eq!(INNER_ENV_VALUE, "1");
        assert_eq!(NET_MODE_ENV, "MANOX_SANDBOX_INNER_NET");
        assert_eq!(BRIDGE_FD_ENV, "MANOX_SANDBOX_INNER_BRIDGE_FD");
    }
}
