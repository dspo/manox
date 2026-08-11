//! FS path policy for the pi toolset: a read deny-list over sensitive user
//! locations and a write-confinement check, enforced by the `ToolCall` hook
//! registered in [`crate::pi_engine`] (pure-Rust checks at the tool entry —
//! the kernel stays untouched).
//!
//! ## Scope and honest gaps
//!
//! - **bash stays approval-gated only.** The retired manox sandbox wrapped
//!   bash in macOS seatbelt; the pi bash tool is a persistent shell confined
//!   by the approval gate alone. A seatbelt (or bwrap) backend for pi bash
//!   is a separate, larger task. FS tools are the primary model-driven
//!   exfiltration vector and are what this policy covers; `bash cat ~/.ssh/…`
//!   still routes through user approval, not this policy.
//! - **Root-level checks only.** The hook sees each tool call's `path`
//!   argument, not the entries a walk descends through, so a `grep`/`find`
//!   rooted at a permitted directory can still *list* names inside a denied
//!   subtree (reading their contents through `Read` is blocked). Walk-level
//!   pruning would need kernel tool changes.
//! - **No project-only read allowlist.** Reads outside the sensitive set stay
//!   permitted (system headers, `/etc/hosts`); the policy blocks high-value
//!   targets (SSH keys, cloud creds, keychains, media libraries) rather than
//!   confining reads to the project root.
//!
//! Strings here are model-visible (block reasons feed back to the LLM) and
//! stay English — they do not go through i18n.

use std::path::{Path, PathBuf};

/// Canonicalize best-effort: an existing path canonicalizes outright; a
/// not-yet-existing path canonicalizes its nearest existing ancestor and
/// re-appends the missing tail, so symlinked roots (`/private/var`) classify
/// correctly even for files the tool is about to create.
pub fn canonicalize_best_effort(path: &Path) -> PathBuf {
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

/// Read deny-list for the FS read tools (`Read`/`Grep`/`Find`/`Ls`). The
/// denied set is the standard sensitive home subtrees plus secret-bearing
/// filenames. Immutable after construction.
#[derive(Clone, Debug)]
pub struct ReadPolicy {
    denied_roots: Vec<PathBuf>,
}

impl ReadPolicy {
    /// Build the default read policy. The denied roots derive from `$HOME`
    /// (best-effort: absent HOME yields an empty denied set rather than a
    /// panic).
    pub fn new() -> Self {
        Self {
            denied_roots: home_denied_roots(),
        }
    }

    /// Canonicalized denied subtrees.
    pub fn denied_roots(&self) -> &[PathBuf] {
        &self.denied_roots
    }

    /// Whether `path` is denied for reading: it falls under a sensitive user
    /// subtree, or its filename matches a secret-bearing pattern.
    pub fn is_denied(&self, path: &Path) -> bool {
        let canon = canonicalize_best_effort(path);
        if self.denied_roots.iter().any(|r| canon.starts_with(r)) {
            return true;
        }
        is_likely_secret_file(&canon)
    }

    /// Check `path`; an `Err` carries the English block reason surfaced to
    /// the model, pointing it at the approval-gated escape hatch (bash).
    pub fn check(&self, path: &Path) -> Result<(), String> {
        if self.is_denied(path) {
            return Err(format!(
                "Read blocked by path policy (sensitive path or secret file): {}. \
                 If you genuinely need it, use the bash tool and pass user approval.",
                path.display()
            ));
        }
        Ok(())
    }
}

impl Default for ReadPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// The canonicalized sensitive subtrees under the user's home directory.
/// Empty when `$HOME` is unset.
fn home_denied_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let candidates = [
        ".ssh",
        ".aws",
        ".gnupg",
        ".config",
        "Library",
        "Music",
        "Pictures",
        "Photos Library.photoslibrary",
    ];
    candidates
        .into_iter()
        .map(|c| canonicalize_best_effort(&home.join(c)))
        .collect()
}

/// Filenames that conventionally hold secrets: `.env` and `.env.*`, private
/// SSH key material, and per-tool credential files. Exact-filename match, so
/// `id_rsa.pub` (public key) is NOT blocked. Cheap filename-only check (no
/// canonicalize) for walk-based pruning.
pub fn is_likely_secret_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }
    matches!(
        name,
        "id_rsa"
            | "id_ed25519"
            | "id_ecdsa"
            | "id_dsa"
            | "id_x25519"
            | ".npmrc"
            | ".pypirc"
            | ".netrc"
            | "credentials"
    )
}

/// Write confinement for the FS write tools (`Write`/`Edit`): writable set =
/// project root + temp scratch + the global plans dir; protected set = any
/// `.git` component (the c5aefe4d escape class: writes into a repo's `.git`
/// from outside the normal git workflow). Pure-Rust check, cross-platform —
/// the retired manox seatbelt wrapper for bash is not part of this slice.
#[derive(Clone, Debug)]
pub struct WritePolicy {
    writable_roots: Vec<PathBuf>,
}

impl WritePolicy {
    /// Writable set for `project_root`: the canonicalized project root, the
    /// system temp dirs (`/tmp` + `/private/tmp` scratch), and the global
    /// plan-file directory (plan mode drafts its plan file there).
    pub fn for_project(project_root: &Path) -> Self {
        let mut writable_roots = vec![canonicalize_best_effort(project_root)];
        writable_roots.push(PathBuf::from("/tmp"));
        writable_roots.push(PathBuf::from("/private/tmp"));
        if let Ok(plans) = crate::paths::plans_dir() {
            writable_roots.push(canonicalize_best_effort(&plans));
        }
        Self { writable_roots }
    }

    /// Whether `path` may be written: under a writable root and not
    /// protected (no `.git` component).
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let canon = canonicalize_best_effort(path);
        if canon
            .components()
            .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
        {
            return Err(format!(
                "Write blocked by path policy (`.git` is protected): {}. \
                 Use git commands through bash (user-approved) for git state.",
                path.display()
            ));
        }
        if self.writable_roots.iter().any(|r| canon.starts_with(r)) {
            return Ok(());
        }
        Err(format!(
            "Write blocked by path policy (outside the project root, temp scratch, and plans dir): {}. \
             Write inside the project, or use bash with user approval.",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
    }

    #[test]
    fn read_policy_denies_sensitive_home_subtrees() {
        let policy = ReadPolicy::new();
        assert!(policy.is_denied(&home().join(".ssh/id_rsa")));
        assert!(policy.is_denied(&home().join(".aws/credentials")));
        assert!(policy.is_denied(&home().join(".config/cx/cx.providers.config.yaml")));
    }

    #[test]
    fn read_policy_allows_project_and_public_keys() {
        let policy = ReadPolicy::new();
        assert!(!policy.is_denied(Path::new("/tmp/manox-policy-test/src/main.rs")));
        // Public key material is NOT secret.
        assert!(!is_likely_secret_file(Path::new("/x/id_rsa.pub")));
    }

    #[test]
    fn read_policy_denies_secret_filenames_anywhere() {
        let policy = ReadPolicy::new();
        assert!(policy.is_denied(Path::new("/tmp/manox-policy-test/.env")));
        assert!(policy.is_denied(Path::new("/tmp/manox-policy-test/.env.local")));
        assert!(policy.is_denied(Path::new("/tmp/manox-policy-test/.netrc")));
        // Ordinary `foo.env` files pass (`.env.` prefix required).
        assert!(!policy.is_denied(Path::new("/tmp/manox-policy-test/send.env")));
    }

    #[test]
    fn read_policy_check_message_is_actionable() {
        let policy = ReadPolicy::new();
        let err = policy.check(&home().join(".ssh/id_rsa")).unwrap_err();
        assert!(err.contains("bash"), "{err}");
        assert!(
            policy
                .check(Path::new("/tmp/manox-policy-test/ok.rs"))
                .is_ok()
        );
    }

    #[test]
    fn write_policy_confines_to_project_temp_plans() {
        let policy = WritePolicy::for_project(Path::new("/tmp/manox-policy-test"));
        assert!(
            policy
                .check(Path::new("/tmp/manox-policy-test/src/lib.rs"))
                .is_ok()
        );
        assert!(policy.check(Path::new("/tmp/scratch-file.txt")).is_ok());
        let plans = crate::paths::plans_dir().unwrap();
        assert!(policy.check(&plans.join("x-plan.md")).is_ok());
        // /tmp scratch is writable by design (scratch space); a path outside
        // project + temp + plans is confined.
        let err = policy
            .check(Path::new("/etc/manox-policy-test/lib.rs"))
            .unwrap_err();
        assert!(err.contains("outside"), "{err}");
    }

    #[test]
    fn write_policy_protects_git_dirs() {
        let policy = WritePolicy::for_project(Path::new("/tmp/manox-policy-test"));
        let err = policy
            .check(Path::new("/tmp/manox-policy-test/.git/config"))
            .unwrap_err();
        assert!(err.contains(".git"), "{err}");
        // A path merely containing "git" in a name is fine.
        assert!(
            policy
                .check(Path::new("/tmp/manox-policy-test/git_notes.md"))
                .is_ok()
        );
    }
}
