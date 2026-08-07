//! Shell kind detection for spawn-time marker injection.
//!
//! The terminal readiness handshake wraps the shell's argv at spawn time: the
//! wrapper prints a private OSC marker, then `exec`s the shell proper.
//! Different shells have different `printf` escape semantics and quoting
//! rules, so the wrapper must be shell-aware. Detection is by program
//! basename; unknown kinds spawn unwrapped and readiness falls back to the
//! output-timing heuristic.
//!
//! The marker is a private OSC sequence: `\x1b]6973;manox-ready=<uuid>\x07`.
//! vte silently ignores unknown OSC codes, so the marker bytes pass through
//! `Processor::advance` without visible artifact. The nonce prevents false
//! positives from user-typed text.

use std::path::Path;

use portable_pty::CommandBuilder;

/// Shell kind for marker command template selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// sh / bash / zsh / dash — all support `printf` octal escapes and the
    /// `-l` login flag.
    Posix,
    Fish,
    PowerShell,
    Nushell,
    Cmd,
}

impl ShellKind {
    /// Detect shell kind from a program path by basename match. Returns
    /// `None` for unknown programs — the caller spawns unwrapped and falls
    /// back to heuristic readiness. ksh is deliberately unrecognized: it has
    /// no portable login-emulation flag, so wrapping it would risk losing
    /// login-shell behavior.
    pub fn detect(program: &str) -> Option<Self> {
        let basename = Path::new(program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(program);
        let name = basename.strip_suffix(".exe").unwrap_or(basename);
        match name {
            "sh" | "bash" | "zsh" | "dash" => Some(Self::Posix),
            "fish" => Some(Self::Fish),
            "pwsh" | "powershell" => Some(Self::PowerShell),
            "nu" => Some(Self::Nushell),
            "cmd" => Some(Self::Cmd),
            _ => None,
        }
    }

    /// Build argv for the spawn wrapper that prints the marker and exec's the
    /// shell. `login` mirrors portable-pty's default-prog behavior (the user's
    /// default shell runs as a login shell; explicit overrides do not).
    /// Returns `None` for kinds without a portable marker template — the
    /// caller falls back to heuristic readiness.
    ///
    /// Posix/Fish: `shell -c "printf '%s' '<ESC>]6973;manox-ready=<uuid><BEL>'; exec '<shell>' [-l]"`
    ///   - ESC / BEL travel as literal bytes inside the single-quoted string;
    ///     argv is byte-transparent after quote removal, so this dodges the
    ///     printf octal-escape differences between dash/bash/zsh/fish.
    ///     `exec` replaces the wrapper, keeping the process tree (and the PID
    ///     for wait/kill) identical to the unwrapped case; `-l` preserves
    ///     login-shell startup files.
    ///
    /// PowerShell: `pwsh -NoLogo -Command "[Console]::Write([char]27+']6973;manox-ready=<uuid>'+[char]7); & '<shell>'"`
    ///   - No `exec` equivalent; the shell runs nested and process-group kill
    ///     covers the tree. Best-effort (macOS-first, untested on CI).
    ///
    /// Nushell: `nu --commands "print $\"(char esc)]6973;manox-ready=<uuid>(char bel)\"; ^'<shell>'"`
    ///   - `^` spawns nested. Best-effort.
    pub fn marker_command(
        &self,
        shell_path: &str,
        nonce: &str,
        login: bool,
    ) -> Option<Vec<String>> {
        match self {
            Self::Posix | Self::Fish => {
                let login_flag = if login { " -l" } else { "" };
                let payload = format!(
                    "printf '%s' '\x1b]6973;manox-ready={nonce}\x07'; exec '{shell_path}'{login_flag}"
                );
                Some(vec![shell_path.to_string(), "-c".to_string(), payload])
            }
            Self::PowerShell => {
                let payload = format!(
                    "[Console]::Write([char]27+']6973;manox-ready={nonce}'+[char]7); & '{shell_path}'"
                );
                Some(vec![
                    shell_path.to_string(),
                    "-NoLogo".to_string(),
                    "-Command".to_string(),
                    payload,
                ])
            }
            Self::Nushell => {
                let payload = format!(
                    "print $\"(char esc)]6973;manox-ready={nonce}(char bel)\"; ^'{shell_path}'"
                );
                Some(vec![
                    shell_path.to_string(),
                    "--commands".to_string(),
                    payload,
                ])
            }
            Self::Cmd => {
                // cmd.exe echo cannot emit a literal ESC portably; spawn
                // unwrapped instead.
                None
            }
        }
    }
}

/// Resolve the shell program path for spawn: the explicit override, else
/// portable-pty's own default-prog resolution ($SHELL → passwd db → /bin/sh)
/// so the wrapped spawn runs exactly the shell the unwrapped path would have.
pub fn resolve_shell_program(override_path: Option<&str>) -> String {
    if let Some(p) = override_path {
        return p.to_string();
    }
    CommandBuilder::new_default_prog().get_shell()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_posix_shells() {
        assert_eq!(ShellKind::detect("/bin/sh"), Some(ShellKind::Posix));
        assert_eq!(ShellKind::detect("/bin/bash"), Some(ShellKind::Posix));
        assert_eq!(ShellKind::detect("/bin/zsh"), Some(ShellKind::Posix));
        assert_eq!(
            ShellKind::detect("/usr/local/bin/dash"),
            Some(ShellKind::Posix)
        );
    }

    #[test]
    fn detect_fish() {
        assert_eq!(
            ShellKind::detect("/usr/local/bin/fish"),
            Some(ShellKind::Fish)
        );
        assert_eq!(ShellKind::detect("fish"), Some(ShellKind::Fish));
    }

    #[test]
    fn detect_powershell() {
        assert_eq!(ShellKind::detect("pwsh"), Some(ShellKind::PowerShell));
        assert_eq!(
            ShellKind::detect("/usr/bin/pwsh"),
            Some(ShellKind::PowerShell)
        );
        assert_eq!(
            ShellKind::detect("powershell.exe"),
            Some(ShellKind::PowerShell)
        );
    }

    #[test]
    fn detect_nushell() {
        assert_eq!(ShellKind::detect("nu"), Some(ShellKind::Nushell));
        assert_eq!(ShellKind::detect("/usr/bin/nu"), Some(ShellKind::Nushell));
    }

    #[test]
    fn detect_cmd() {
        assert_eq!(ShellKind::detect("cmd"), Some(ShellKind::Cmd));
        assert_eq!(ShellKind::detect("cmd.exe"), Some(ShellKind::Cmd));
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(ShellKind::detect("vim"), None);
        assert_eq!(ShellKind::detect("xonsh"), None);
        // ksh has no portable login-emulation flag; left unwrapped on purpose.
        assert_eq!(ShellKind::detect("ksh"), None);
    }

    #[test]
    fn posix_marker_command_login() {
        let argv = ShellKind::Posix
            .marker_command("/bin/zsh", "abc-123", true)
            .unwrap();
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0], "/bin/zsh");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("printf '%s' '\x1b]6973;manox-ready=abc-123\x07'"));
        assert!(argv[2].ends_with("exec '/bin/zsh' -l"));
    }

    #[test]
    fn posix_marker_command_non_login() {
        let argv = ShellKind::Posix
            .marker_command("/bin/bash", "abc-123", false)
            .unwrap();
        assert!(argv[2].ends_with("exec '/bin/bash'"));
    }

    #[test]
    fn fish_marker_command_format() {
        let argv = ShellKind::Fish
            .marker_command("/usr/bin/fish", "xyz-456", true)
            .unwrap();
        assert_eq!(argv[0], "/usr/bin/fish");
        assert!(argv[2].contains("printf '%s' '\x1b]6973;manox-ready=xyz-456\x07'"));
        assert!(argv[2].ends_with("exec '/usr/bin/fish' -l"));
    }

    #[test]
    fn powershell_marker_command_format() {
        let argv = ShellKind::PowerShell
            .marker_command("pwsh", "nonce-789", false)
            .unwrap();
        assert_eq!(argv[0], "pwsh");
        assert_eq!(argv[1], "-NoLogo");
        assert_eq!(argv[2], "-Command");
        assert!(argv[3].contains("[char]27+']6973;manox-ready=nonce-789'"));
    }

    #[test]
    fn nushell_marker_command_format() {
        let argv = ShellKind::Nushell
            .marker_command("nu", "nonce-abc", false)
            .unwrap();
        assert_eq!(argv[0], "nu");
        assert_eq!(argv[1], "--commands");
        assert!(argv[2].contains("manox-ready=nonce-abc"));
    }

    #[test]
    fn cmd_falls_back_to_heuristic() {
        assert!(
            ShellKind::Cmd
                .marker_command("cmd", "nonce", false)
                .is_none()
        );
    }

    #[test]
    fn override_resolution_is_verbatim() {
        assert_eq!(resolve_shell_program(Some("/bin/zsh")), "/bin/zsh");
    }
}
