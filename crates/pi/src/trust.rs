// Trust manager — project trust decisions.
//
// Before the agent executes tools in a project, the user must trust the
// project directory. Trust decisions are persisted and can be revoked.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

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
#[derive(Debug, Default)]
pub struct TrustManager {
    trusted: HashSet<PathBuf>,
}

impl TrustManager {
    /// Create a new trust manager.
    pub fn new() -> Self {
        TrustManager::default()
    }

    /// Check the trust status of a directory.
    pub fn check(&self, path: &PathBuf) -> TrustStatus {
        // Normalize the path for consistent comparison.
        let normalized = normalize_path(path);
        if self.trusted.contains(&normalized) {
            TrustStatus::Trusted
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
    pub fn revoke(&mut self, path: &PathBuf) {
        let normalized = normalize_path(path);
        self.trusted.remove(&normalized);
    }

    /// Whether a directory is trusted.
    pub fn is_trusted(&self, path: &PathBuf) -> bool {
        matches!(self.check(path), TrustStatus::Trusted)
    }
}

/// Normalize a path for consistent trust lookups.
fn normalize_path(path: &PathBuf) -> PathBuf {
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
        assert_eq!(
            tm.check(&PathBuf::from("/test")),
            TrustStatus::Undecided
        );
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