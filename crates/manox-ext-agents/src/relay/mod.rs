pub mod ipc;
pub mod pty;
pub mod transfer;

use crate::warp::WarpSession;
use portable_pty::ExitStatus as PtyExitStatus;

/// Build the relay-time env slice (currently just the Warp session id, if any).
pub fn warp_env(warp_session: &Option<WarpSession>) -> Vec<(&'static str, String)> {
    match warp_session {
        Some(ws) => vec![("CX_WARP_SESSION_ID", ws.session_id().to_string())],
        None => Vec::new(),
    }
}

/// Map a `portable_pty::ExitStatus` to a shell-style exit code.
///
/// `portable_pty` collapses signal deaths to `exit_code()==1` and keeps only the
/// `strsignal` description (e.g. "Terminated"), so the 128+signal convention is
/// best-effort; unmapped signals fall back to the reported exit code.
pub fn pty_exit_code(status: &PtyExitStatus) -> i32 {
    if status.success() {
        return 0;
    }
    if let Some(sig) = status.signal()
        && let Some(n) = signal_number(sig)
    {
        return 128 + n;
    }
    status.exit_code() as i32
}

/// Best-effort `strsignal` description → signal number for the common cases.
fn signal_number(desc: &str) -> Option<i32> {
    match desc {
        "Hangup" => Some(libc::SIGHUP),
        "Interrupt" => Some(libc::SIGINT),
        "Quit" => Some(libc::SIGQUIT),
        "Killed" => Some(libc::SIGKILL),
        "Segmentation fault" | "Segmentation Fault" => Some(libc::SIGSEGV),
        "Terminated" => Some(libc::SIGTERM),
        _ => None,
    }
}
