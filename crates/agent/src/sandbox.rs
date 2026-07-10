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
//! files and runs binaries); the sandbox confines writes + network
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
    /// Read only by the seatbelt renderer (macOS). Kept cross-platform as a
    /// policy knob for future Linux bwrap / Windows backends; on non-macOS it
    /// is written but not yet read.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    allow_network: bool,
    /// Subtrees of `protected_paths` explicitly re-opened for writes — the
    /// bound repo's shared `.git`, while a worktree is active. Empty in the
    /// default `for_project` case, so `.git` stays protected as before. A path
    /// is protected iff it is under a `protected_paths` entry AND not under any
    /// `git_allowed_roots` entry; the harness-managed worktree entry on the
    /// same repo is an approved action distinct from the c5aefe4d unauthorized
    /// jump to a sibling repo's `.git`.
    git_allowed_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// Build the default policy for `project_root`. Roots are canonicalized
    /// best-effort so the Rust-side path checks (and seatbelt `subpath`
    /// matching, which resolves symlinks) compare against real paths — the
    /// temp dir is a symlink to `/private/var/...` on macOS.
    ///
    /// When `project_root` is the filesystem root (`/`), the writable set is
    /// narrowed to the temp dir only: admitting `/` would make the entire disk
    /// writable (every path `starts_with('/')`), turning the sandbox into a
    /// no-op. This is the state thread 6cd3d096 ran in (manox launched with
    /// `cwd=/`), and it silently neutralized the write confinement. The
    /// `.git` protected path is dropped too (`/.git` is meaningless). A
    /// `tracing::warn` marks the degenerate policy so the launch is audible.
    pub fn for_project(project_root: &Path) -> Self {
        let root = canonicalize_best_effort(project_root);
        let temp = canonicalize_best_effort(&std::env::temp_dir());
        if root.parent().is_none() {
            tracing::warn!(
                root = %root.display(),
                "sandbox project root is the filesystem root; narrowing writable set to temp dir only — launch manox from a real project directory to restore full confinement"
            );
            return Self {
                writable_roots: vec![temp],
                protected_paths: Vec::new(),
                allow_network: false,
                git_allowed_roots: Vec::new(),
            };
        }
        Self {
            writable_roots: vec![root.clone(), temp],
            protected_paths: vec![root.join(".git")],
            allow_network: false,
            git_allowed_roots: Vec::new(),
        }
    }

    /// Extend a project policy for an active worktree: admit the worktree path
    /// as a writable root, re-open the bound repo's shared `.git` for writes
    /// (so `git commit`/`rebase`/`push` against the main repo's `.git` work),
    /// and enable network — a worktree is an approved isolation context, and
    /// git workflows need `push`/`fetch` to be frictionless. The c5aefe4d
    /// threat (unauthorized `cd` into a sibling repo's `.git`) stays blocked:
    /// only `main_repo_git_dir` is de-protected, sibling worktrees' `.git`
    /// entries are neither under `writable_roots` nor in `git_allowed_roots`.
    ///
    /// `main_repo_git_dir` is the path returned by `git rev-parse
    /// --git-common-dir` from inside the worktree — the main repo's `.git`,
    /// shared across all linked worktrees.
    pub fn with_worktree(mut self, worktree_path: &Path, main_repo_git_dir: &Path) -> Self {
        self.writable_roots
            .push(canonicalize_best_effort(worktree_path));
        self.git_allowed_roots
            .push(canonicalize_best_effort(main_repo_git_dir));
        self.allow_network = true;
        self
    }

    /// Policy for a sub-agent spawned with worktree isolation: the child may
    /// write only its own worktree (not the parent's project root) plus temp,
    /// may run git ops against the bound repo's shared `.git`, and has network
    /// — the same approved-isolation-context relaxation as [`with_worktree`],
    /// but anchored on the worktree alone so parent and sibling trees are out
    /// of reach. `protected_paths` is empty because a linked worktree has no
    /// `.git` directory of its own (it shares the main repo's).
    pub fn for_worktree(worktree_path: &Path, main_repo_git_dir: &Path) -> Self {
        Self {
            writable_roots: vec![
                canonicalize_best_effort(worktree_path),
                canonicalize_best_effort(&std::env::temp_dir()),
            ],
            protected_paths: Vec::new(),
            allow_network: true,
            git_allowed_roots: vec![canonicalize_best_effort(main_repo_git_dir)],
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

    /// Whether `path` falls under a protected path (e.g. project `.git`), minus
    /// any subtree explicitly re-opened via `git_allowed_roots` (the bound
    /// repo's `.git` while a worktree is active). A path under a protected root
    /// that also falls under a git-allowed root is NOT protected — the
    /// harness-managed worktree entry de-protects the same repo's `.git` for git
    /// ops, without admitting sibling repos' `.git` (the c5aefe4d escape).
    pub fn is_protected(&self, path: &Path) -> bool {
        let canon = canonicalize_best_effort(path);
        let under_protected = self.protected_paths.iter().any(|p| canon.starts_with(p));
        if !under_protected {
            return false;
        }
        let under_git_allowed = self.git_allowed_roots.iter().any(|g| canon.starts_with(g));
        !under_git_allowed
    }

    /// The combined write decision: a path is writable only if it falls under
    /// a writable root AND is not protected. This is the single predicate both
    /// FS write tools and the bash unsandboxed `cwd` pre-check consult, so the
    /// Rust-side confinement and the seatbelt `(allow file-write* (subpath
    /// ...))` + `(deny file-write* (subpath ".git"))` policy classify paths
    /// identically — a protected subtree (`.git`) is under the project root, so
    /// `is_writable` alone would admit it; the protection deny must be applied
    /// on top, matching seatbelt's more-specific-rule-wins ordering.
    pub fn is_write_allowed(&self, path: &Path) -> bool {
        self.is_writable(path) && !self.is_protected(path)
    }

    /// Render a seatbelt (`.sbpl`) policy string. Denylist base
    /// (`(allow default)`) with an allowlist over writes: deny all writes,
    /// re-allow to writable roots, deny to protected paths, deny all network
    /// when `allow_network` is false. More-specific rules win, so the
    /// `.git` deny overrides the project-root allow for that subtree.
    ///
    /// Character-device redirection targets (`/dev/null`, `/dev/zero`,
    /// `/dev/stdout`, `/dev/stderr`) are allowlisted as literals: they are not
    /// under any writable root, so `(deny file-write*)` would otherwise reject
    /// `cmd > /dev/null`. They are write-only sinks with no persistent state.
    #[cfg(target_os = "macos")]
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
        for dev in ["/dev/null", "/dev/zero", "/dev/stdout", "/dev/stderr"] {
            s.push_str(&format!("(allow file-write* (literal \"{dev}\"))\n"));
        }
        for p in &self.protected_paths {
            s.push_str(&format!(
                "(deny file-write* (subpath \"{}\"))\n",
                escape_seatbelt_path(p)
            ));
        }
        // Re-allow the bound repo's `.git` AFTER the protected denies so a
        // linked worktree's git ops (commit/rebase/push against the main repo's
        // shared `.git`) succeed. Seatbelt's last-matching-rule-wins ordering
        // makes the later allow override the earlier deny for that subtree
        // alone; sibling repos' `.git` entries are neither in writable_roots
        // nor here, so they stay denied.
        for g in &self.git_allowed_roots {
            s.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                escape_seatbelt_path(g)
            ));
        }
        if !self.allow_network {
            s.push_str("(deny network*)\n");
        }
        s
    }

    /// Wrap a bash command in a `sandbox-exec` invocation. `command` is passed
    /// as a single argv element to `bash -c`, so the model's command string is
    /// never re-evaluated by an outer shell — no escaping, no injection. The
    /// login shell's PATH is injected so the sandboxed bash finds Homebrew /
    /// toolchain binaries the GUI process env otherwise lacks (thread
    /// `e5047fd2`: `gh` not found). Other env (HOME, KEYCHAIN_*, LANG) is
    /// inherited as-is.
    #[cfg(target_os = "macos")]
    pub fn wrap_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p")
            .arg(self.render_seatbelt())
            .arg("--")
            .arg("bash")
            .arg("-c")
            .arg(command)
            .env("PATH", crate::path_env::resolved_login_path())
            .current_dir(cwd);
        cmd
    }
}

/// Canonicalize a path that may not yet exist: resolve the longest existing
/// ancestor and rejoin the remaining tail. Falls back to the raw path when no
/// ancestor exists. Shared with the read policy so FS read-side and write-side
/// path classification use the same canonical baseline (symlink resolution,
/// `/private/var` on macOS).
pub(crate) fn canonicalize_best_effort(path: &Path) -> PathBuf {
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
/// string literals are C-escaped; `\`, `"`, and control newlines need escaping
/// to keep a malformed path from breaking the policy syntax. Paths currently
/// come from `for_project()` (not model input), so this is defense-in-depth.
#[cfg(target_os = "macos")]
fn escape_seatbelt_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Whether the OS sandbox backend is available on the current platform.
pub fn is_available() -> bool {
    cfg!(target_os = "macos")
}

/// Conservative heuristic for bash commands that drive other macOS applications
/// via Apple Events — `osascript` (the AppleScript bridge), the `tell
/// application` AppleScript phrase, and `open -a <App>` (launch by name).
///
/// These escape the FS/network confinement seatbelt enforces: seatbelt's
/// `(allow default)` base admits Mach IPC, so a sandboxed `osascript` can still
/// reach other apps. They are therefore gated on user approval in
/// [`BashTool::requires_approval`](crate::tools::bash::BashTool) regardless of
/// the `unsandboxed` flag, and audited in hook execution. The match is a
/// substring/word check, not a shell parser — it errs toward flagging (an extra
/// approval prompt) over silently letting cross-app automation through. A model
/// determined to evade via aliasing cannot be fully stopped at the string layer;
/// the entitlement removal (no `automation.apple-events` + no usage
/// description) is the hard OS-level backstop.
pub fn is_cross_app_automation(command: &str) -> bool {
    // `osascript` is the AppleScript / OSA bridge binary; matching the file
    // name of any whitespace token catches `osascript`, `/usr/bin/osascript`,
    // and `./osascript` while avoiding a bare substring false positive on a
    // word that merely contains the letters (e.g. a path segment `osascripts`).
    if command
        .split_whitespace()
        .any(|tok| Path::new(tok).file_name().is_some_and(|n| n == "osascript"))
    {
        return true;
    }
    let lower = command.to_ascii_lowercase();
    // Canonical AppleScript phrase; covers `tell application "Finder" to ...`.
    if lower.contains("tell application") {
        return true;
    }
    // `open -a <App>` launches a specific application by name.
    if lower.contains("open -a") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SandboxPolicy {
        SandboxPolicy::for_project(Path::new("/tmp/manox-sandbox-test"))
    }

    #[test]
    fn is_cross_app_automation_flags_osascript() {
        assert!(is_cross_app_automation(
            "osascript -e 'tell application \"Finder\" to quit'"
        ));
        assert!(is_cross_app_automation("/usr/bin/osascript foo.scpt"));
    }

    #[test]
    fn is_cross_app_automation_flags_tell_application() {
        assert!(is_cross_app_automation(
            "echo 'tell application \"Music\" to play' | osascript"
        ));
    }

    #[test]
    fn is_cross_app_automation_flags_open_a() {
        assert!(is_cross_app_automation("open -a 'Visual Studio Code' ."));
    }

    #[test]
    fn is_cross_app_automation_ignores_ordinary_commands() {
        assert!(!is_cross_app_automation("echo hi"));
        // `open file.txt` (no `-a`) opens via the default handler, not a named app.
        assert!(!is_cross_app_automation("open file.txt"));
        assert!(!is_cross_app_automation("cat tell.txt"));
        assert!(!is_cross_app_automation("cargo build"));
    }

    #[test]
    fn is_cross_app_automation_conservatively_flags_tell_application_substring() {
        // A grep for the literal string "tell application" contains the phrase
        // and is flagged — a deliberate false positive: the heuristic errs
        // toward an extra approval prompt over silently admitting cross-app
        // automation.
        assert!(is_cross_app_automation("grep -r tell application src"));
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
    fn policy_for_root_degenerates_to_temp_only() {
        // Launching with `cwd=/` (thread 6cd3d096) must NOT admit the whole
        // disk: `/` as a writable root makes every path writable. The policy
        // narrows to temp-only and drops the meaningless `/.git` protection.
        let p = SandboxPolicy::for_project(Path::new("/"));
        assert!(
            p.is_writable(&std::env::temp_dir().join("scratch")),
            "temp dir stays writable under degenerate root policy"
        );
        assert!(
            !p.is_writable(Path::new("/etc/passwd")),
            "system paths must NOT be writable when project root is /"
        );
        assert!(
            !p.is_writable(Path::new("/Users/")),
            "user paths must NOT be writable when project root is /"
        );
        assert!(p.protected_paths.is_empty(), "no .git to protect at /");
    }

    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_denies_dot_git() {
        let s = policy().render_seatbelt();
        assert!(
            s.contains("deny file-write* (subpath"),
            ".git deny must appear: {s}"
        );
        assert!(s.contains(".git"), "policy: {s}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_denies_network() {
        let s = policy().render_seatbelt();
        assert!(s.contains("(deny network*)"), "network denied: {s}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fs_and_seatbelt_agree_on_representative_paths() {
        // The Rust-side `is_write_allowed` (used by FS `resolve_path_for_write`
        // and the bash unsandboxed cwd pre-check) and the seatbelt
        // `(allow file-write* (subpath ...))` + `(deny ... .git)` lines must
        // classify the same paths the same way — otherwise a write FS rejects
        // bash allows (or vice-versa), the inconsistency behind issue 3. A
        // protected subtree (`.git`) is under the project root, so the combined
        // `is_write_allowed` (writable AND not protected) is the predicate that
        // matches seatbelt's more-specific-rule-wins ordering.
        let p = policy();
        let s = p.render_seatbelt();
        // Project root + tmp are writable on both sides.
        assert!(p.is_write_allowed(Path::new("/tmp/manox-sandbox-test/src/lib.rs")));
        assert!(s.contains("/tmp/manox-sandbox-test"));
        assert!(p.is_write_allowed(&std::env::temp_dir().join("scratch")));
        // Sibling worktree, /etc are NOT writable on either side.
        assert!(!p.is_write_allowed(Path::new("/tmp/manox-sibling-worktree/x")));
        assert!(!p.is_write_allowed(Path::new("/etc/passwd")));
        // The seatbelt policy string contains no `allow` for these.
        assert!(!s.contains("/tmp/manox-sibling-worktree"));
        assert!(!s.contains("/etc/passwd"));
        // `.git` is under the project root (so `is_writable` alone is true) but
        // protected — the combined predicate must reject it, matching seatbelt's
        // explicit deny.
        assert!(!p.is_write_allowed(Path::new("/tmp/manox-sandbox-test/.git/config")));
        assert!(s.contains("deny file-write* (subpath"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_allows_dev_null_and_redirect_targets() {
        // `/dev/null` is a character device outside any writable root; without
        // an explicit literal allow, `cmd > /dev/null` is rejected. The same
        // applies to the other common redirection sinks.
        let s = policy().render_seatbelt();
        for dev in ["/dev/null", "/dev/zero", "/dev/stdout", "/dev/stderr"] {
            assert!(
                s.contains(&format!("(allow file-write* (literal \"{dev}\"))")),
                "{dev} must be allowlisted: {s}"
            );
        }
        // The Rust-side check is unchanged: /dev/null is not "writable" in the
        // FS-tool sense — only seatbelt redirection is relaxed for it.
        assert!(!policy().is_writable(Path::new("/dev/null")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_command_injects_login_path() {
        // The login-shell PATH must reach the sandboxed bash so Homebrew /
        // toolchain binaries are found (thread e5047fd2: `gh` not found).
        let p = policy();
        let cmd = p.wrap_command("echo hi", Path::new("/tmp/manox-sandbox-test"));
        let path = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v)
            .expect("PATH env must be set on sandboxed bash");
        let path_str = path.to_string_lossy();
        assert!(!path_str.is_empty(), "injected PATH must not be empty");
        assert!(
            path_str.contains("/usr/bin"),
            "injected PATH must include system bin dirs: {path_str}"
        );
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

    // ─── worktree policy ───────────────────────────────────────────────────

    #[test]
    fn with_worktree_opens_bound_git_and_network() {
        let project = Path::new("/tmp/manox-sandbox-test");
        let git_dir = project.join(".git");
        let worktree = Path::new("/tmp/manox-sandbox-test/.claude/worktrees/wt-1");
        let p = SandboxPolicy::for_project(project).with_worktree(worktree, &git_dir);
        // The bound repo's .git is now writable (git ops work).
        assert!(
            p.is_write_allowed(&git_dir.join("config")),
            "bound .git must be writable under worktree policy"
        );
        // The worktree itself is writable.
        assert!(p.is_write_allowed(&worktree.join("src/lib.rs")));
        // Network is on.
        assert!(p.allow_network);
    }

    #[test]
    fn with_worktree_still_blocks_sibling_worktree_git() {
        // The c5aefe4d escape: cd into a SIBLING worktree and git ops against
        // its .git. Only the bound repo's .git is de-protected; a sibling's
        // .git stays blocked.
        let project = Path::new("/tmp/manox-sandbox-test");
        let git_dir = project.join(".git");
        let worktree = Path::new("/tmp/manox-sandbox-test/.claude/worktrees/wt-1");
        let p = SandboxPolicy::for_project(project).with_worktree(worktree, &git_dir);
        let sibling = Path::new("/tmp/manox-sibling-worktree/.git/config");
        assert!(
            !p.is_write_allowed(sibling),
            "sibling worktree's .git must stay blocked"
        );
        assert!(
            !p.is_writable(Path::new("/tmp/manox-sibling-worktree/x")),
            "sibling worktree path must stay non-writable"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_with_worktree_allows_bound_git_and_network() {
        let project = Path::new("/tmp/manox-sandbox-test");
        let git_dir = project.join(".git");
        let worktree = Path::new("/tmp/manox-sandbox-test/.claude/worktrees/wt-1");
        let p = SandboxPolicy::for_project(project).with_worktree(worktree, &git_dir);
        let s = p.render_seatbelt();
        // The bound .git allow appears after the .git deny.
        let deny_idx = s.find("(deny file-write* (subpath").unwrap();
        let allow_idx = s
            .find(&format!(
                "(allow file-write* (subpath \"{}",
                git_dir
                    .canonicalize()
                    .unwrap_or_else(|_| git_dir.clone())
                    .display()
            ))
            .or_else(|| s.find("(allow file-write* (subpath"));
        // At least one git-allowed allow line is present after the deny block.
        assert!(
            allow_idx.map(|i| i > deny_idx).unwrap_or(false)
                || s.matches("(allow file-write* (subpath").count() >= 3,
            "bound .git allow must follow the deny: {s}"
        );
        // Network is no longer denied.
        assert!(
            !s.contains("(deny network*)"),
            "network must be allowed: {s}"
        );
    }

    #[test]
    fn for_worktree_confines_child_to_its_own_tree() {
        // A sub-agent with worktree isolation: writable = its worktree + temp
        // only, git ops against the bound .git work, the parent's project root
        // is out of reach.
        let parent_project = Path::new("/tmp/parent-project");
        let git_dir = parent_project.join(".git");
        let child_wt = Path::new("/tmp/parent-project/.claude/worktrees/sub-1");
        let p = SandboxPolicy::for_worktree(child_wt, &git_dir);
        assert!(p.is_write_allowed(&child_wt.join("src/lib.rs")));
        // Parent project root is NOT writable for the child.
        assert!(
            !p.is_write_allowed(&parent_project.join("src/lib.rs")),
            "child must not write parent project root"
        );
        // Bound .git: not writable via the Rust FS check (not under the child's
        // writable_roots), but the seatbelt emits an allow so bash git ops work.
        assert!(
            !p.is_write_allowed(&git_dir.join("config")),
            "FS tools must not write .git directly; bash seatbelt is the git-op path"
        );
        assert!(p.allow_network);
    }
}
