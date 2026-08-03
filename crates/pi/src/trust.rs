// Trust manager — project trust decisions.
//
// Before the agent executes tools in a project, the user must trust the
// project directory. Trust decisions are persisted and can be revoked.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The trust status of a project directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustStatus {
    /// The project is trusted.
    Trusted,
    /// The project is not trusted.
    Untrusted,
    /// No trust decision has been made yet.
    Undecided,
}

/// Manages trust decisions for project directories.
///
/// Decisions persist to a JSON file (`{trusted: [...], untrusted: [...]}`)
/// so a reopened session keeps its policy; the manager only records state —
/// applying it to resource/tool enablement is the caller's job.
#[derive(Debug, Default)]
pub struct TrustManager {
    trusted: HashSet<PathBuf>,
    untrusted: HashSet<PathBuf>,
}

impl TrustManager {
    /// Create a new trust manager.
    pub fn new() -> Self {
        TrustManager::default()
    }

    /// Check the trust status of a directory.
    pub fn check(&self, path: &Path) -> TrustStatus {
        // Normalize the path for consistent comparison.
        let normalized = normalize_path(path);
        if self.trusted.contains(&normalized) {
            TrustStatus::Trusted
        } else if self.untrusted.contains(&normalized) {
            TrustStatus::Untrusted
        } else {
            TrustStatus::Undecided
        }
    }

    /// Mark a directory as trusted.
    pub fn trust(&mut self, path: PathBuf) {
        let normalized = normalize_path(&path);
        self.trusted.insert(normalized);
    }

    /// Revoke trust for a directory.
    pub fn revoke(&mut self, path: &Path) {
        let normalized = normalize_path(path);
        self.trusted.remove(&normalized);
        self.untrusted.remove(&normalized);
    }

    /// Mark a directory as untrusted.
    pub fn untrust(&mut self, path: PathBuf) {
        let normalized = normalize_path(&path);
        self.trusted.remove(&normalized);
        self.untrusted.insert(normalized);
    }

    /// Whether a directory is trusted.
    pub fn is_trusted(&self, path: &Path) -> bool {
        matches!(self.check(path), TrustStatus::Trusted)
    }

    /// Load trust decisions from a JSON file. A missing file reads as no
    /// decisions; a corrupt file is refused rather than silently reset.
    pub fn load(path: &Path) -> Result<Self, anyhow::Error> {
        match std::fs::read_to_string(path) {
            Ok(json) => {
                #[derive(serde::Deserialize)]
                struct Wire {
                    #[serde(default)]
                    trusted: Vec<PathBuf>,
                    #[serde(default)]
                    untrusted: Vec<PathBuf>,
                }
                let wire: Wire = serde_json::from_str(&json)
                    .map_err(|e| anyhow::anyhow!("invalid trust file {}: {e}", path.display()))?;
                Ok(TrustManager {
                    trusted: wire
                        .trusted
                        .into_iter()
                        .map(|p| normalize_path(&p))
                        .collect(),
                    untrusted: wire
                        .untrusted
                        .into_iter()
                        .map(|p| normalize_path(&p))
                        .collect(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustManager::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist trust decisions to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let wire = serde_json::json!({
            "trusted": self.trusted.iter().collect::<Vec<_>>(),
            "untrusted": self.untrusted.iter().collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_string_pretty(&wire)?)?;
        Ok(())
    }
}

/// Normalize a path for consistent trust lookups.
fn normalize_path(path: &Path) -> PathBuf {
    // Canonicalize if possible, otherwise clean up the path.
    path.canonicalize().unwrap_or_else(|_| {
        // Fallback: clean up redundant separators and dots.
        let mut cleaned = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    cleaned.pop();
                }
                std::path::Component::CurDir => {}
                c => {
                    cleaned.push(c);
                }
            }
        }
        cleaned
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_trust_manager() {
        let tm = TrustManager::new();
        assert_eq!(tm.check(&PathBuf::from("/test")), TrustStatus::Undecided);
    }

    #[test]
    fn test_persist_roundtrip_preserves_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trust.json");
        let mut tm = TrustManager::new();
        tm.trust("/proj/a".into());
        tm.untrust("/proj/b".into());
        tm.save(&file).unwrap();

        let loaded = TrustManager::load(&file).unwrap();
        assert_eq!(loaded.check(Path::new("/proj/a")), TrustStatus::Trusted);
        assert_eq!(loaded.check(Path::new("/proj/b")), TrustStatus::Untrusted);
        assert_eq!(loaded.check(Path::new("/proj/c")), TrustStatus::Undecided);
    }

    #[test]
    fn test_load_missing_file_is_no_decisions() {
        let loaded = TrustManager::load(Path::new("/nonexistent/trust.json")).unwrap();
        assert_eq!(loaded.check(Path::new("/proj")), TrustStatus::Undecided);
    }

    #[test]
    fn test_trust_and_revoke() {
        let mut tm = TrustManager::new();
        let path = PathBuf::from("/test/project");

        assert!(!tm.is_trusted(&path));

        tm.trust(path.clone());
        assert!(tm.is_trusted(&path));
        assert_eq!(tm.check(&path), TrustStatus::Trusted);

        tm.revoke(&path);
        assert!(!tm.is_trusted(&path));
        assert_eq!(tm.check(&path), TrustStatus::Undecided);
    }
}
