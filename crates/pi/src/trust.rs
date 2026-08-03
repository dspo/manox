// Trust manager — project trust decisions, mirroring the TS
// `ProjectTrustStore`: a global `agentDir/trust.json` mapping canonical
// paths to `true | false`, resolved by walking the cwd up to the nearest
// ancestor with an explicit decision. Trust gates project config resources
// (`.pi/skills`, `.pi/prompts`, ...); an undecided project is treated as
// untrusted until the caller (UI) records a decision.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// Manages trust decisions for project directories, persisted as a map of
/// canonical path → decision (TS `agentDir/trust.json`).
#[derive(Debug, Clone, Default)]
pub struct TrustManager {
    decisions: HashMap<PathBuf, bool>,
}

impl TrustManager {
    pub fn new() -> Self {
        TrustManager::default()
    }

    /// The trust status of a directory: the nearest ancestor (itself
    /// included) with an explicit decision wins; no decision anywhere on the
    /// path reads as undecided.
    pub fn check(&self, cwd: &Path) -> TrustStatus {
        let mut dir = normalize_path(cwd);
        loop {
            if let Some(decision) = self.decisions.get(&dir) {
                return if *decision {
                    TrustStatus::Trusted
                } else {
                    TrustStatus::Untrusted
                };
            }
            let parent = dir.parent();
            let Some(parent) = parent else { break };
            if parent == dir {
                break;
            }
            dir = parent.to_path_buf();
        }
        TrustStatus::Undecided
    }

    /// Mark a directory as trusted.
    pub fn trust(&mut self, path: &Path) {
        self.decisions.insert(normalize_path(path), true);
    }

    /// Mark a directory as untrusted.
    pub fn untrust(&mut self, path: &Path) {
        self.decisions.insert(normalize_path(path), false);
    }

    /// Clear a directory's decision (`null` in the TS store).
    pub fn clear(&mut self, path: &Path) {
        self.decisions.remove(&normalize_path(path));
    }

    /// Whether a directory is trusted (nearest-ancestor resolution).
    pub fn is_trusted(&self, path: &Path) -> bool {
        matches!(self.check(path), TrustStatus::Trusted)
    }

    /// Load decisions from the TS-shaped JSON file (`{path: bool}`). A
    /// missing file reads as no decisions; a corrupt file is refused rather
    /// than silently reset.
    pub fn load(path: &Path) -> Result<Self, anyhow::Error> {
        match std::fs::read_to_string(path) {
            Ok(json) => {
                let wire: HashMap<String, bool> = serde_json::from_str(&json)
                    .map_err(|e| anyhow::anyhow!("invalid trust file {}: {e}", path.display()))?;
                Ok(TrustManager {
                    decisions: wire
                        .into_iter()
                        .map(|(k, v)| (PathBuf::from(k), v))
                        .collect(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustManager::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist decisions to the TS-shaped JSON file, keys sorted.
    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut sorted: Vec<(PathBuf, bool)> = self
            .decisions
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let wire: HashMap<&Path, bool> = sorted.iter().map(|(k, v)| (k.as_path(), *v)).collect();
        std::fs::write(path, serde_json::to_string_pretty(&wire)?)?;
        Ok(())
    }
}

/// Normalize a path for consistent trust lookups (canonical when possible).
fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
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

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn nearest_ancestor_decision_wins() {
        let mut tm = TrustManager::new();
        tm.untrust(Path::new("/proj"));
        // A child of an untrusted root inherits the decision.
        assert_eq!(tm.check(Path::new("/proj/sub/dir")), TrustStatus::Untrusted);
        // An explicit decision on the child overrides the ancestor.
        tm.trust(Path::new("/proj/sub"));
        assert_eq!(tm.check(Path::new("/proj/sub")), TrustStatus::Trusted);
        assert_eq!(tm.check(Path::new("/other")), TrustStatus::Undecided);
    }

    #[test]
    fn persist_roundtrip_preserves_decisions() {
        let dir = tmp();
        let file = dir.path().join("trust.json");
        let mut tm = TrustManager::new();
        tm.trust(Path::new("/proj/a"));
        tm.untrust(Path::new("/proj/b"));
        tm.save(&file).unwrap();

        let loaded = TrustManager::load(&file).unwrap();
        assert_eq!(loaded.check(Path::new("/proj/a")), TrustStatus::Trusted);
        assert_eq!(loaded.check(Path::new("/proj/b")), TrustStatus::Untrusted);
        assert_eq!(loaded.check(Path::new("/proj/c")), TrustStatus::Undecided);
    }

    #[test]
    fn load_missing_file_is_no_decisions() {
        let loaded = TrustManager::load(Path::new("/nonexistent/trust.json")).unwrap();
        assert_eq!(loaded.check(Path::new("/proj")), TrustStatus::Undecided);
    }
}
