//! FS read-side path policy for the read-only tools (`read_file`, `grep`,
//! `glob`, `list_directory`). A deny-list over sensitive user directories and
//! secret-bearing filenames, applied as a pure-Rust check at the tool entry
//! points. It does not touch the macOS seatbelt (`(allow default)` admits reads).
//!
//! ## Scope and honest gaps
//!
//! This is the staged first slice of a tighter read profile, not a full
//! read-deny:
//! - **bash reads stay unrestricted.** The seatbelt profile is a denylist over
//!   non-write syscalls; adding a full read-deny to seatbelt is a large,
//!   separate change (bash legitimately reads system files and runs binaries).
//!   FS read tools are the primary exfiltration vector from the model, and
//!   they are what this policy covers; bash `cat ~/.ssh/id_rsa` is still a gap.
//! - **No project-only allowlist.** Reads outside the sensitive set remain
//!   permitted (e.g. `/etc/hosts`, system headers) — a project-only allow mode
//!   is future work. This policy blocks the high-value targets (SSH keys,
//!   cloud creds, keychains, media libraries) rather than confining reads to
//!   the project root.
//!
//! Strings here are model-visible (tool errors feed back to the LLM) and so
//! stay English — they do not go through i18n.

use std::path::{Path, PathBuf};

/// Deny policy for the read-only FS tools. Derived from the project root and
/// the user's home directory; the denied set is the standard sensitive
/// locations plus secret-bearing filenames. Immutable after construction.
#[derive(Clone, Debug)]
pub struct ReadPolicy {
    project_root: PathBuf,
    denied_roots: Vec<PathBuf>,
}

impl ReadPolicy {
    /// Build the default read policy for `project_root`. The denied roots are
    /// derived from `$HOME` (best-effort: absent HOME yields an empty denied
    /// set rather than a panic — the project root check still applies).
    pub fn for_project(project_root: &Path) -> Self {
        let root = crate::sandbox::canonicalize_best_effort(project_root);
        let denied_roots = home_denied_roots();
        Self {
            project_root: root,
            denied_roots,
        }
    }

    /// The canonicalized project root reads stay permitted under.
    #[allow(dead_code)]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Canonicalized denied subtrees — exposed so `grep`/`glob` walks can
    /// `filter_entry`-prune them when descending from a parent (e.g. a search
    /// rooted at `$HOME`).
    pub fn denied_roots(&self) -> &[PathBuf] {
        &self.denied_roots
    }

    /// Whether `path` is denied for reading: it falls under a sensitive user
    /// subtree, or its filename matches a secret-bearing pattern. The path is
    /// canonicalized best-effort first so symlinked roots (`/private/var`) and
    /// not-yet-existing paths classify against the canonical denied roots.
    pub fn is_denied(&self, path: &Path) -> bool {
        let canon = crate::sandbox::canonicalize_best_effort(path);
        if self.denied_roots.iter().any(|r| canon.starts_with(r)) {
            return true;
        }
        is_likely_secret_file(&canon)
    }

    /// Check `path` and return an English error string when denied, for the
    /// read tools to surface to the model. The message points the model at the
    /// approval-gated escape hatch (`bash` with `unsandboxed: true`) so a
    /// legitimate need is routed through user consent rather than silently
    /// failing.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        if self.is_denied(path) {
            return Err(format!(
                "Read blocked by read policy (sensitive path or secret file): {}. \
                 To read it, set `unsandboxed: true` in the bash tool and pass user approval.",
                path.display()
            ));
        }
        Ok(())
    }
}

/// The canonicalized sensitive subtrees under the user's home directory:
/// `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config`, `~/Library`, `~/Music`,
/// `~/Pictures`, and the Photos library package. Empty when `$HOME` is unset.
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
        .map(|c| crate::sandbox::canonicalize_best_effort(&home.join(c)))
        .collect()
}

/// Filenames that conventionally hold secrets: `.env` and `.env.*`, private SSH
/// key material, and per-tool credential files. Matches by exact filename, so
/// `id_rsa.pub` (public key) is NOT blocked — only the private key names.
fn is_likely_secret_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if name == ".env" {
        return true;
    }
    // `.env.local`, `.env.production`, etc. (but not `send.env` or a file named
    // `x.env` — require the `.env.` prefix so ordinary `foo.env` files pass).
    if name.starts_with(".env.") {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_for_tmp_project() -> ReadPolicy {
        ReadPolicy::for_project(Path::new("/tmp/manox-read-policy-test"))
    }

    fn home() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
    }

    #[test]
    fn project_root_read_allowed() {
        let p = policy_for_tmp_project();
        assert!(
            p.check(Path::new("/tmp/manox-read-policy-test/src/lib.rs"))
                .is_ok()
        );
    }

    #[test]
    fn system_file_read_allowed() {
        // Non-sensitive system files are still readable; this policy is a
        // deny-list, not a project-only allowlist.
        let p = policy_for_tmp_project();
        // /etc/hosts may not exist in every test sandbox; check() only
        // classifies by path, not existence.
        assert!(p.check(Path::new("/etc/hosts")).is_ok());
        assert!(p.check(Path::new("/usr/share/dict/words")).is_ok());
    }

    #[test]
    fn ssh_dir_denied() {
        let p = policy_for_tmp_project();
        assert!(p.is_denied(&home().join(".ssh/id_rsa")));
        assert!(p.is_denied(&home().join(".ssh/config")));
    }

    #[test]
    fn cloud_and_gnupg_dirs_denied() {
        let p = policy_for_tmp_project();
        assert!(p.is_denied(&home().join(".aws/credentials")));
        assert!(p.is_denied(&home().join(".aws/config")));
        assert!(p.is_denied(&home().join(".gnupg/secring.gpg")));
    }

    #[test]
    fn config_dir_denied() {
        let p = policy_for_tmp_project();
        // The entire ~/.config subtree is denied — manox's own provider config
        // lives there and may carry API keys.
        assert!(p.is_denied(&home().join(".config/cx/cx.providers.config.yaml")));
    }

    #[test]
    fn library_and_media_dirs_denied() {
        let p = policy_for_tmp_project();
        assert!(p.is_denied(&home().join("Library/Preferences/com.apple.plist")));
        assert!(p.is_denied(&home().join("Music/track.m4a")));
        assert!(p.is_denied(&home().join("Pictures/photo.jpg")));
        assert!(p.is_denied(&home().join("Photos Library.photoslibrary/database")));
    }

    #[test]
    fn secret_filenames_denied_anywhere() {
        let p = policy_for_tmp_project();
        // A `.env` in the project root is still blocked — the model should not
        // exfiltrate project-local secrets either.
        assert!(p.is_denied(Path::new("/tmp/manox-read-policy-test/.env")));
        assert!(p.is_denied(Path::new("/tmp/manox-read-policy-test/.env.local")));
        assert!(p.is_denied(Path::new("/tmp/manox-read-policy-test/secrets/id_rsa")));
        assert!(p.is_denied(Path::new("/tmp/manox-read-policy-test/.npmrc")));
    }

    #[test]
    fn public_keys_and_env_named_files_pass() {
        let p = policy_for_tmp_project();
        // `id_rsa.pub` is a public key — not secret.
        assert!(!p.is_denied(Path::new("/tmp/manox-read-policy-test/id_rsa.pub")));
        // `foo.env` is not the `.env` convention.
        assert!(!p.is_denied(Path::new("/tmp/manox-read-policy-test/foo.env")));
        assert!(!p.is_denied(Path::new("/tmp/manox-read-policy-test/README.md")));
    }

    #[test]
    fn denied_roots_exposed_for_walk_pruning() {
        let p = policy_for_tmp_project();
        let roots = p.denied_roots();
        assert!(
            !roots.is_empty(),
            "HOME-derived denied roots should be present"
        );
    }
}
