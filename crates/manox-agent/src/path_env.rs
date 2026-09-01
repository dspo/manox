//! Login-shell `PATH` installation — tool subprocesses see the same binaries
//! the user sees in an interactive terminal, not the minimal PATH the GUI
//! process inherits from launchd.
//!
//! manox is launched by macOS launchd (or a GUI launcher), so its process
//! PATH is the system default (`/usr/bin:/bin:/usr/sbin:/sbin`) without
//! Homebrew (`/opt/homebrew/bin`) or any user-added entries. Every
//! subprocess inheriting that PATH loses `gh`, `rg`, `fd`, etc. — the
//! failure that motivated the retired manox harness's `path_env` module.
//!
//! The pi path has a single injection point: the process environment itself.
//! Every spawn site inherits it — the pi kernel's bash tool
//! (`TokioExecutionEnv`), LSP servers (`crates/lsp`), MCP stdio servers
//! (supervisor bus), and monitor/background commands (pi-extensions).
//! [`install`] resolves the login shell's PATH once on a background thread
//! and applies it process-wide, so no kernel or extension changes are
//! needed.
//!
//! `$SHELL -l -c 'printf %s "$PATH"'` re-runs the user's login shell in
//! login mode so `.zprofile` / `.zshrc` (or `.bash_profile`) apply — the
//! exact files that append Homebrew and toolchain paths. Resolution is
//! bounded ([`RESOLVE_TIMEOUT`]; a hung dotfile must not wedge the install)
//! and any failure applies a conservative default so the agent still runs.

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Conservative fallback when the login shell query fails (e.g. SHELL unset,
/// shell binary missing, query hit the deadline). Covers Homebrew on both
/// arches, standard system dirs, and `/usr/local/bin` for manual installs.
pub const DEFAULT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Deadline for the login-shell query: a misbehaving profile must not keep
/// the PATH install pending forever.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

/// The PATH resolution is done; `setenv` has been applied process-wide.
/// Used by downstream init (LSP probe, MCP spawns) to wait for the correct
/// PATH before probing.
static INSTALLED: OnceLock<()> = OnceLock::new();

static RESOLVED: OnceLock<String> = OnceLock::new();

/// The login shell's PATH, resolved once and cached. Falls back to
/// [`DEFAULT_PATH`] when resolution fails — never panics, never returns
/// empty.
pub fn resolved_login_path() -> &'static str {
    RESOLVED.get_or_init(resolve_fallible).as_str()
}

/// Install the login shell's PATH process-wide from a background thread.
///
/// Called once from `manox_agent::init`. When the resolver returns, the process
/// `PATH` is replaced, so every later subprocess spawn (bash tool, LSP
/// servers, MCP stdio servers, monitors) inherits the user's environment.
/// Subprocesses spawned before the resolver lands inherit the launcher's
/// minimal PATH — the pre-install situation, never worse.
pub fn install() {
    let started = std::thread::Builder::new()
        .name("path-env-install".into())
        .spawn(|| {
            let path = resolved_login_path();
            // SAFETY: `set_var` is unsafe on edition 2024 because a
            // concurrent `getenv` in another thread could race the environ
            // update. This runs on a dedicated thread at the earliest init
            // moment (before any engine/runtime work is spawned by manox),
            // and the only readers of `PATH` afterwards are subprocess
            // spawn sites, which is the exact contract this install exists
            // to serve. The race window can reach [`RESOLVE_TIMEOUT`] when
            // the login-shell probe is slow, but a concurrent `getenv` only
            // observes the old PATH in the interim — never a torn value —
            // so accepting it is the trade for a single injection point
            // instead of patching every spawn site in the workspace.
            unsafe { std::env::set_var("PATH", path) };
            INSTALLED.set(()).ok();
            tracing::debug!(len = path.len(), "login shell PATH installed");
        })
        .is_ok();
    if !started {
        tracing::warn!("path-env install thread failed to start; keeping launcher PATH");
    }
}

/// Block until the login-shell PATH is installed (bounded by the
/// [`RESOLVE_TIMEOUT`] inside the install thread).  Used by init steps
/// that probe PATH (LSP registry, MCP server discovery) and must not
/// see the launchd minimal PATH.
pub fn wait_installed() {
    while INSTALLED.get().is_none() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn resolve_fallible() -> String {
    match try_resolve_login_path() {
        Some(p) if !p.is_empty() => p,
        _ => DEFAULT_PATH.to_string(),
    }
}

/// Run `$SHELL -l -c 'printf %s "$PATH"'` and return its stdout. `None` on
/// any error (no SHELL, spawn failure, non-zero exit, deadline hit).
/// Best-effort: the caller always has the default to fall back to.
fn try_resolve_login_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    // `printf %s "$PATH"` avoids a trailing newline, so the captured stdout
    // is exactly PATH with no trimming needed. `-l` makes the shell a login
    // shell so the profile files that append Homebrew / toolchain paths are
    // sourced.
    let mut child = std::process::Command::new(&shell)
        .arg("-l")
        .arg("-c")
        .arg("printf %s \"$PATH\"")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + RESOLVE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    };
    if !status.success() {
        return None;
    }
    // PATH is small (well under the pipe buffer), so reading after exit is
    // safe from pipe-fill deadlock.
    let mut path = String::new();
    child.stdout.take()?.read_to_string(&mut path).ok()?;
    if path.is_empty() {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_login_path_is_nonempty_and_contains_system_bin() {
        // Either the real login PATH or the fallback — both contain /usr/bin.
        let p = resolved_login_path();
        assert!(!p.is_empty(), "PATH must never be empty");
        assert!(
            p.contains("/usr/bin"),
            "PATH must include system bin dirs: {p}"
        );
    }

    #[test]
    fn fallback_default_contains_homebrew() {
        // The fallback must cover Homebrew on Apple Silicon — the most
        // common missing entry in the launchd PATH that motivated this
        // module.
        assert!(DEFAULT_PATH.contains("/opt/homebrew/bin"));
    }

    #[test]
    fn install_applies_resolved_path_process_wide() {
        install();
        let expected = resolved_login_path().to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::env::var("PATH").ok().as_deref() == Some(expected.as_str()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "install did not apply the resolved PATH within 5s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
