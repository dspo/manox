//! In-memory snapshot store keyed by `(path, tag)`.
//!
//! Each path retains up to `MAX_VERSIONS_PER_PATH` historical snapshots (tail =
//! most recent); the store caps total tracked paths at `MAX_PATHS` via LRU
//! eviction of the least-recently-touched path. Snapshots are session-scoped:
//! a read mints a tag, an edit validates against it, and a successful edit
//! records a fresh head snapshot so the next edit can chain on the returned tag.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::hash::compute_tag;

/// Maximum versions retained per path (LRU eviction drops the head/oldest).
const MAX_VERSIONS_PER_PATH: usize = 4;
/// Maximum distinct paths tracked before the least-recently-touched is evicted.
const MAX_PATHS: usize = 30;

/// A recorded file snapshot: its normalized text and the tag derived from it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: PathBuf,
    pub text: String,
    pub tag: String,
}

/// Session-scoped store of file snapshots, keyed by path with per-path version
/// history. Interior mutability is provided by the global `Mutex` in
/// [`super::global`], not here.
#[derive(Debug, Default)]
pub struct SnapshotStore {
    by_path: HashMap<PathBuf, Vec<Snapshot>>,
    /// Insertion/recency order of paths; tail is most-recently-touched. Used
    /// for LRU eviction when the path count exceeds `MAX_PATHS`.
    path_order: Vec<PathBuf>,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a snapshot for `path` from raw `text`. Computes the tag, appends
    /// a new version (evicting the oldest if over the per-path cap), refreshes
    /// path recency, and returns the recorded snapshot.
    pub fn record(&mut self, path: &Path, text: &str) -> Snapshot {
        let tag = compute_tag(text);
        let snap = Snapshot {
            path: path.to_path_buf(),
            text: text.to_string(),
            tag,
        };

        let versions = self.by_path.entry(path.to_path_buf()).or_default();
        // De-duplicate byte-identical re-reads: refresh recency without growing
        // history if the head already matches.
        if versions.last().is_some_and(|h| h.tag == snap.tag) {
            versions.last_mut().unwrap().text = snap.text.clone();
        } else {
            versions.push(snap.clone());
            if versions.len() > MAX_VERSIONS_PER_PATH {
                versions.remove(0);
            }
        }

        self.touch_path(path);
        snap
    }

    /// Look up a historical snapshot by `(path, tag)`. Does not refresh recency.
    pub fn get(&self, path: &Path, tag: &str) -> Option<&Snapshot> {
        self.by_path
            .get(path)
            .and_then(|versions| versions.iter().find(|s| s.tag == tag))
    }

    /// The most recently recorded snapshot for `path`, if any.
    pub fn head(&self, path: &Path) -> Option<&Snapshot> {
        self.by_path.get(path).and_then(|v| v.last())
    }

    fn touch_path(&mut self, path: &Path) {
        self.path_order.retain(|p| p != path);
        self.path_order.push(path.to_path_buf());
        while self.path_order.len() > MAX_PATHS {
            let evicted = self.path_order.remove(0);
            self.by_path.remove(&evicted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn record_and_head() {
        let mut store = SnapshotStore::new();
        let snap = store.record(&p("a.rs"), "fn main() {}\n");
        assert_eq!(snap.tag, compute_tag("fn main() {}\n"));
        assert_eq!(store.head(&p("a.rs")).unwrap().tag, snap.tag);
    }

    #[test]
    fn get_finds_historical_tag() {
        let mut store = SnapshotStore::new();
        let v1 = store.record(&p("a.rs"), "a\n");
        let v2 = store.record(&p("a.rs"), "b\n");
        assert_ne!(v1.tag, v2.tag);
        assert_eq!(store.get(&p("a.rs"), &v1.tag).unwrap().text, "a\n");
        assert_eq!(store.get(&p("a.rs"), &v2.tag).unwrap().text, "b\n");
    }

    #[test]
    fn dedup_identical_reread() {
        let mut store = SnapshotStore::new();
        store.record(&p("a.rs"), "x\n");
        store.record(&p("a.rs"), "x\n");
        // Identical re-read must not grow version history.
        assert_eq!(store.by_path.get(&p("a.rs")).map(|v| v.len()), Some(1));
    }

    #[test]
    fn per_path_version_cap() {
        let mut store = SnapshotStore::new();
        for i in 0..(MAX_VERSIONS_PER_PATH + 2) {
            store.record(&p("a.rs"), &format!("v{i}\n"));
        }
        assert_eq!(
            store.by_path.get(&p("a.rs")).map(|v| v.len()).unwrap(),
            MAX_VERSIONS_PER_PATH
        );
    }

    #[test]
    fn global_path_cap_evicts_lru() {
        let mut store = SnapshotStore::new();
        for i in 0..(MAX_PATHS + 2) {
            store.record(&p(&format!("file{i}.rs")), "x\n");
        }
        assert!(store.by_path.len() <= MAX_PATHS);
        // The earliest-recorded paths should have been evicted.
        assert!(store.head(&p("file0.rs")).is_none());
    }
}
