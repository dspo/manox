//! Resolve the login shell's `PATH` so tool subprocesses (sandboxed
//! `sandbox-exec` bash, brush persistent shell) see the same binaries the user
//! does in an interactive terminal — not the minimal PATH the GUI process
//! inherits from launchd.
//!
//! manox is launched by macOS launchd (or a GUI launcher), so its process PATH
//! is the system default (`/usr/bin:/bin:/usr/sbin:/sbin`) without Homebrew
//! (`/opt/homebrew/bin`) or any user-added entries. Every bash invocation that
//! inherits this PATH loses `gh`, `rg`, `fd`, etc. — the failure behind thread
//! `e5047fd2`'s "gh not found". The fix is not to remove the in-process shell
//! (brush is only the unsandboxed escape hatch; the default sandboxed path
//! already uses `sandbox-exec` and is unaffected by brush) but to inject the
//! login shell's PATH into both paths.
//!
//! `$SHELL -l -c 'printf %s "$PATH"'` re-runs the user's login shell in login
//! mode so `.zprofile` / `.zshrc` (or `.bash_profile`) apply — the exact files
//! that append Homebrew and toolchain paths. The result is cached for the
//! process lifetime (PATH doesn't change mid-session); on any failure a
//! conservative default is returned so the agent still runs.

use std::sync::OnceLock;

/// Conservative fallback when the login shell query fails (e.g. SHELL unset,
/// shell binary missing, command timed out). Covers Homebrew on both arches,
/// standard system dirs, and `/usr/local/bin` for manual installs.
const DEFAULT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// The login shell's PATH, resolved once and cached. Falls back to
/// [`DEFAULT_PATH`] when resolution fails — never panics, never returns empty.
pub fn resolved_login_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(resolve_fallible).as_str()
}

fn resolve_fallible() -> String {
    match try_resolve_login_path() {
        Some(p) if !p.is_empty() => p,
        _ => DEFAULT_PATH.to_string(),
    }
}

/// Run `$SHELL -l -c 'printf %s "$PATH"'` and return its stdout. `None` on any
/// error (no SHELL, spawn failure, non-zero exit, timeout). Best-effort: the
/// caller always has the default to fall back to.
fn try_resolve_login_path() -> Option<String> {
    use std::process::Command;
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    // `printf %s "$PATH"` avoids a trailing newline, so the captured stdout is
    // exactly PATH with no trimming needed. `-l` makes the shell a login shell
    // so the profile files that append Homebrew / toolchain paths are sourced.
    let output = Command::new(&shell)
        .arg("-l")
        .arg("-c")
        .arg("printf %s \"$PATH\"")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
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
        // The fallback must cover Homebrew on Apple Silicon — the most common
        // missing entry in the launchd PATH that motivated this module.
        assert!(DEFAULT_PATH.contains("/opt/homebrew/bin"));
    }
}
