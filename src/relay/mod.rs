// PTY relay: cx owns the master, the slave becomes the agent's controlling
// terminal, and the user terminal is bridged to it byte-for-byte. A Unix socket
// per session lets external processes inject messages (see `cx send`).
//
// This module is the CLI driver over `api::SessionHandle`: it prints the banner,
// advertises the socket, enters raw mode, forwards stdin, pumps master output
// to stdout, and finalizes on EOF — returning `!` via `process::exit`. The
// handle owns the spawn/IPC/writer-thread lifecycle; `relay::run` owns the
// terminal interaction.

pub(crate) mod ipc;
pub(crate) mod pty;
pub(crate) mod transfer;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::ExitStatus as PtyExitStatus;
use signal_hook::consts::signal::SIGWINCH;
use signal_hook::flag as sig_flag;

use crate::LaunchSpec;
use crate::api::SessionHandle;
use crate::warp::WarpSession;

use transfer::stdin_forward;

/// Run the agent through a PTY relay, accepting IPC injections.
///
/// Owns the terminal interaction and returns `!` via `process::exit`. The
/// spawn/IPC/writer lifecycle is delegated to `SessionHandle`.
pub(crate) fn run(spec: &LaunchSpec, warp_session: Option<WarpSession>) -> ! {
    let started_sys = SystemTime::now();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Launch banner in cooked mode before raw mode swallows the newlines. The
    // socket line is appended once the handle has bound its IPC socket (below),
    // so only a live socket is advertised; a trailing blank separates the banner
    // from the agent's raw output.
    println!();
    println!("{}", spec.summary);
    let _ = std::io::stdout().flush();

    let handle = match SessionHandle::spawn(spec, warp_session) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("cx: {e:#}");
            std::process::exit(1);
        }
    };

    // Advertise the injection socket while still in cooked mode, before raw mode
    // swallows the output. Only a successfully bound socket is advertised — a
    // failed bind prints nothing rather than a dead path. External processes
    // (e.g. `cx send`) connect here to inject messages as if the user typed them.
    if let Some(sock) = handle.socket_path() {
        println!("socket: {}", sock.display());
        let _ = std::io::stdout().flush();
    }

    // Blank separator between the banner and the agent's raw output, flushed so
    // it lands before raw mode takes over.
    println!();
    let _ = std::io::stdout().flush();

    // Enter raw mode so keystrokes pass through verbatim (Ctrl+C = 0x03, not SIGINT).
    if let Err(e) = enable_raw_mode() {
        eprintln!("cx: 进入 raw mode 失败: {e}");
        let res = handle.kill_wait();
        print_exit_summary(&res, started_sys, &cwd);
        std::process::exit(res.exit_code);
    }
    let _raw_guard = RawGuard;

    // SIGWINCH → resize flag, polled by the writer loop.
    let resize_flag: Arc<AtomicBool> = handle.resize_flag();
    if let Err(e) = sig_flag::register(SIGWINCH, resize_flag) {
        eprintln!("cx: 注册 SIGWINCH 失败（窗口缩放将不生效）: {e}");
    }

    stdin_forward(handle.writer_tx());

    // Pump master output to stdout, flushing per write so the agent's TUI renders
    // without buffering stalls. EOF (read 0) means the agent closed its stdout.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 8192];
    loop {
        match handle.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let _ = out.write_all(&buf[..n]);
                let _ = out.flush();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("cx: master 读取失败: {e}");
                break;
            }
        }
    }

    // Reap the child, clean up IPC, emit Warp stop. The handle owns all of this.
    // After EOF the agent is exiting; `wait` blocks until it does (mirrors the
    // pre-refactor `child.wait()`).
    let res = match handle.wait() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cx: reap 失败: {e:#}");
            std::process::exit(1);
        }
    };
    let _ = disable_raw_mode();
    print_exit_summary(&res, started_sys, &cwd);
    std::process::exit(res.exit_code);
}

/// Print the inline exit summary (agent/provider/model/duration/tokens/termination),
/// matching `finalize_exit_common`'s formatting for the direct-launch path.
fn print_exit_summary(res: &crate::SessionResult, started_sys: SystemTime, cwd: &std::path::Path) {
    let tokens = crate::stats::count_recent_session_tokens(&res.agent_id, started_sys, cwd);
    println!();
    println!(
        "{}",
        crate::format_exit_summary_inline(
            &res.agent_id,
            &res.provider_name,
            res.model_id.as_deref(),
            res.duration,
            res.termination.as_deref(),
            tokens.as_ref(),
        )
    );
    println!();
}

/// Build the relay-time env slice (currently just the Warp session id, if any).
pub(crate) fn warp_env(warp_session: &Option<WarpSession>) -> Vec<(&'static str, String)> {
    match warp_session {
        Some(ws) => vec![("CX_WARP_SESSION_ID", ws.session_id().to_string())],
        None => Vec::new(),
    }
}

/// Restore the terminal on panic; the normal path disables raw mode explicitly
/// before finalizing. `disable_raw_mode` is idempotent, so a double call is harmless.
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Map a `portable_pty::ExitStatus` to a shell-style exit code.
///
/// `portable_pty` collapses signal deaths to `exit_code()==1` and keeps only the
/// `strsignal` description (e.g. "Terminated"), so the 128+signal convention is
/// best-effort; unmapped signals fall back to the reported exit code.
pub(crate) fn pty_exit_code(status: &PtyExitStatus) -> i32 {
    if status.success() {
        return 0;
    }
    if let Some(sig) = status.signal() {
        if let Some(n) = signal_number(sig) {
            return 128 + n;
        }
    }
    status.exit_code() as i32
}

/// Best-effort `strsignal` description → signal number for the common cases.
fn signal_number(desc: &str) -> Option<i32> {
    // strsignal returns localized descriptions; match the C-locale strings.
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
