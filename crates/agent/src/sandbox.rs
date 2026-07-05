//! OS-level sandbox for the `bash` tool: macOS seatbelt (`sandbox-exec`) by
//! default, with a cross-platform Rust path-write check layered onto FS tools.
//!
//! ## Threat model
//!
//! Thread `c5aefe4d` escaped the prior brush-only bash: `cd` into a sibling
//! worktree and `git commit`/`rebase`/`push` against its `.git` — no
//! confinement. The sandbox blocks exactly that class: writes outside the
//! project root + temp dir, writes to `.git`, and all network. Reads and
//! process execution stay unrestricted (the model legitimately reads system
//! files and runs binaries); matching zed/codex which confine writes + network
//! but not reads.
//!
//! ## Backend
//!
//! macOS: [`SandboxPolicy::wrap_command`] wraps the command in
//! `sandbox-exec -p POLICY -- bash -c "<command>"`. The command is a single
//! argv element — zero shell escaping, no injection surface — and seatbelt's
//! process-level inheritance covers bash and every descendant. The
//! `unsandboxed: true` knob (see `tools/bash.rs`) routes through brush's
//! persistent shell instead, gated by user approval.
//!
//! ## Honest gaps
//!
//! - Linux/Windows: [`is_available`] returns false; `bash` falls back to brush
//!   with a `tracing::warn`, and FS write confinement (pure Rust) still applies.
//! - The seatbelt policy is a denylist over non-write syscalls (`(allow
//!   default)` base) and an allowlist over writes (`deny file-write*` +
//!   narrow `allow` for writable roots + `deny` for protected paths). A
//!   stricter `(deny default)` allowlist would need enumerating every syscall
//!   class bash touches and is future work.
//! - `unsandboxed: true` after approval runs entirely outside the sandbox
//!   (brush, no restrictions) — an intentional escape hatch, user-gated.

use std::path::{Path, PathBuf};

/// Confinement policy for one sandboxed invocation. Derived from the project
/// root; the writable set is the project root plus the system temp dir, the
/// protected set is the project's `.git`.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    writable_roots: Vec<PathBuf>,
    protected_paths: Vec<PathBuf>,
    allow_network: bool,
}

impl SandboxPolicy {
    /// Build the default policy for `project_root`. Roots are canonicalized
    /// best-effort so the Rust-side path checks (and seatbelt `subpath`
    /// matching, which resolves symlinks) compare against real paths — the
    /// temp dir is a symlink to `/private/var/...` on macOS.
    pub fn for_project(project_root: &Path) -> Self {
        let root = canonicalize_best_effort(project_root);
        Self {
            writable_roots: vec![
                root.clone(),
                canonicalize_best_effort(&std::env::temp_dir()),
            ],
            protected_paths: vec![root.join(".git")],
            allow_network: false,
        }
    }

    /// Whether `path` falls under a writable root. The candidate may not
    /// exist yet (a file about to be created), so it is canonicalized
    /// best-effort: the longest existing ancestor is resolved and the
    /// remaining components rejoined — otherwise a non-existent path like
    /// `/var/folders/.../T/scratch` would miss the canonicalized root
    /// `/private/var/folders/.../T`.
    pub fn is_writable(&self, path: &Path) -> bool {
        let canon = canonicalize_best_effort(path);
        self.writable_roots
            .iter()
            .any(|root| canon.starts_with(root))
    }

    /// Whether `path` falls under a protected path (e.g. project `.git`).
    pub fn is_protected(&self, path: &Path) -> bool {
        let canon = canonicalize_best_effort(path);
        self.protected_paths.iter().any(|p| canon.starts_with(p))
    }

    /// Render a seatbelt (`.sbpl`) policy string. Denylist base
    /// (`(allow default)`) with an allowlist over writes: deny all writes,
    /// re-allow to writable roots, deny to protected paths, deny all network
    /// when `allow_network` is false. More-specific rules win, so the
    /// `.git` deny overrides the project-root allow for that subtree.
    fn render_seatbelt(&self) -> String {
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
        for p in &self.protected_paths {
            s.push_str(&format!(
                "(deny file-write* (subpath \"{}\"))\n",
                escape_seatbelt_path(p)
            ));
        }
        if !self.allow_network {
            s.push_str("(deny network*)\n");
        }
        s
    }

    /// Wrap a bash command in a `sandbox-exec` invocation. `command` is passed
    /// as a single argv element to `bash -c`, so the model's command string is
    /// never re-evaluated by an outer shell — no escaping, no injection.
    #[cfg(target_os = "macos")]
    pub fn wrap_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p")
            .arg(self.render_seatbelt())
            .arg("--")
            .arg("bash")
            .arg("-c")
            .arg(command)
            .current_dir(cwd);
        cmd
    }
}

/// Canonicalize a path that may not yet exist: resolve the longest existing
/// ancestor and rejoin the remaining tail. Falls back to the raw path when no
/// ancestor exists.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
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

/// Escape a path for a seatbelt `(subpath "...")` string literal. Seatbelt
/// string literals are C-escaped; only `\` and `"` need escaping in real paths.
fn escape_seatbelt_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Whether the OS sandbox backend is available on the current platform.
pub fn is_available() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SandboxPolicy {
        SandboxPolicy::for_project(Path::new("/tmp/manox-sandbox-test"))
    }

    #[test]
    fn policy_for_project_sets_writable_and_protected() {
        let p = policy();
        assert!(p.is_writable(Path::new("/tmp/manox-sandbox-test/src/lib.rs")));
        assert!(p.is_writable(&std::env::temp_dir().join("scratch")));
        assert!(!p.is_writable(Path::new("/etc/passwd")));
    }

    #[test]
    fn policy_protects_dot_git() {
        let p = policy();
        assert!(
            p.is_protected(Path::new("/tmp/manox-sandbox-test/.git/config")),
            ".git must be protected"
        );
    }

    #[test]
    fn seatbelt_allows_project_root_and_tmp() {
        let s = policy().render_seatbelt();
        assert!(s.contains("(allow default)"));
        assert!(s.contains("(deny file-write*)"));
        assert!(s.contains("allow file-write* (subpath"));
        // Both writable roots appear as allow subpaths.
        let tmp = std::env::temp_dir().canonicalize().unwrap_or_default();
        assert!(
            s.contains(&tmp.display().to_string()),
            "temp dir must be writable: {s}"
        );
    }

    #[test]
    fn seatbelt_denies_dot_git() {
        let s = policy().render_seatbelt();
        assert!(
            s.contains("deny file-write* (subpath"),
            ".git deny must appear: {s}"
        );
        assert!(s.contains(".git"), "policy: {s}");
    }

    #[test]
    fn seatbelt_denies_network() {
        let s = policy().render_seatbelt();
        assert!(s.contains("(deny network*)"), "network denied: {s}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_command_uses_sandbox_exec_argv() {
        let p = policy();
        let cmd = p.wrap_command("git status", Path::new("/tmp/manox-sandbox-test"));
        let prog = cmd.as_std().get_program();
        assert_eq!(prog, "/usr/bin/sandbox-exec");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        // -p POLICY -- bash -c "git status"
        assert_eq!(args[0], "-p");
        assert!(args[1].contains("(allow default)"));
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "bash");
        assert_eq!(args[4], "-c");
        assert_eq!(args[5], "git status");
    }
}
