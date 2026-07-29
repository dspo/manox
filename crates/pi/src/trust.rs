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
    pub fn check(&self, _path: &PathBuf) -> TrustStatus {
        TrustStatus::Undecided
    }

    /// Mark a directory as trusted.
    pub fn trust(&mut self, _path: PathBuf) {
        // Placeholder
    }

    /// Revoke trust for a directory.
    pub fn revoke(&mut self, _path: &PathBuf) {
        // Placeholder
    }
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
}