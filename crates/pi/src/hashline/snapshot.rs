//! In-memory snapshot store keyed by path with per-path version history.
//!
//! Each path retains up to `MAX_VERSIONS_PER_PATH` historical snapshots (head =
//! most recent); the store caps total tracked paths at `MAX_PATHS` via LRU
//! eviction, and caps total retained text across all paths at `MAX_TOTAL_BYTES`.
//! Snapshots are session-scoped: a read mints a tag, an edit validates against
//! it, and a successful edit records a fresh head snapshot so the next edit can
//! chain on the returned tag.
//!
//! Two distinct texts that collide on the 6-hex tag are retained as separate
//! versions so callers can still tell them apart via `Snapshot.text` — the tag
//! is only a fast index, never the identity.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::hash::compute_tag;

/// Maximum versions retained per path (LRU eviction drops the oldest).
const MAX_VERSIONS_PER_PATH: usize = 4;
/// Maximum distinct paths tracked before the least-recently-touched is evicted.
const MAX_PATHS: usize = 30;
/// Global ceiling on retained snapshot text across all paths (UTF-8 bytes).
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// A recorded file snapshot: its normalized text, the tag derived from it,
/// and optional set of lines the read tool displayed.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: PathBuf,
    pub text: String,
    pub tag: String,
    pub recorded_at: i64,
    /// 1-indexed lines a producer (read/search) actually displayed under this
    /// tag. `None` means "no provenance recorded".
    pub seen_lines: Option<HashSet<usize>>,
}

/// Session-scoped store of file snapshots, keyed by path with per-path version
/// history. Interior mutability is supplied by the owner (a `Mutex` on
/// `tool::ToolState`), not by this type.
#[derive(Debug, Default)]
pub struct SnapshotStore {
    by_path: HashMap<PathBuf, Vec<Snapshot>>,
    /// Insertion/recency order of paths; tail is most-recently-touched. Used
    /// for LRU eviction when the path count exceeds `MAX_PATHS` or total
    /// retained bytes exceed `MAX_TOTAL_BYTES`.
    path_order: Vec<PathBuf>,
    /// Sum of `text.len()` across all retained snapshots.
    total_bytes: usize,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a snapshot for `path` from normalized `text`. Computes the tag,
    /// appends a new version (evicting the oldest if over the per-path cap),
    /// refreshes path recency, and returns the recorded snapshot.
    ///
    /// De-duplication checks both `hash` AND `text` equality: two distinct
    /// texts that collide on the 6-hex tag are different snapshots — fusing
    /// them under one entry would corrupt seen-lines.
    pub fn record(&mut self, path: &Path, text: &str) -> Snapshot {
        let tag = compute_tag(text);
        let now = chrono::Utc::now().timestamp_millis();

        let versions = self.by_path.entry(path.to_path_buf()).or_default();

        // De-duplicate: same hash AND same text means identical content.
        if let Some(existing) = versions.iter_mut().find(|s| s.tag == tag && s.text == text) {
            existing.recorded_at = now;
            // Promote to head: remove and re-push.
            let snap = existing.clone();
            versions.retain(|s| s.tag != tag || s.text != text);
            versions.push(snap.clone());
            self.touch_path(path);
            return snap;
        }

        let snap = Snapshot {
            path: path.to_path_buf(),
            text: text.to_string(),
            tag,
            recorded_at: now,
            seen_lines: None,
        };

        self.total_bytes += text.len();
        versions.push(snap.clone());
        if versions.len() > MAX_VERSIONS_PER_PATH {
            if let Some(evicted) = versions.first() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.text.len());
            }
            versions.remove(0);
        }

        self.touch_path(path);
        self.evict_if_over_limit();
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

    /// Look up a snapshot by `(path, full_text)` — exact text match. Does not
    /// refresh recency. Returns `None` when no version matches.
    pub fn by_content(&self, path: &Path, full_text: &str) -> Option<&Snapshot> {
        self.by_path
            .get(path)
            .and_then(|versions| versions.iter().find(|s| s.text == full_text))
    }

    /// Find every retained snapshot whose tag equals `hash`, across all tracked
    /// paths. Used for tag-based path recovery (model mistyped the path).
    pub fn find_by_hash(&self, hash: &str) -> Vec<&Snapshot> {
        let mut matches = Vec::new();
        for versions in self.by_path.values() {
            for version in versions {
                if version.tag == hash {
                    matches.push(version);
                }
            }
        }
        matches
    }

    /// Record which lines of a snapshot were displayed by a read tool. Merges
    /// into the existing `seen_lines` set. No-op when the tag is not retained.
    pub fn record_seen_lines(&mut self, path: &Path, tag: &str, lines: &HashSet<usize>) {
        let Some(versions) = self.by_path.get_mut(path) else {
            return;
        };
        let Some(snapshot) = versions.iter_mut().find(|s| s.tag == tag) else {
            return;
        };
        if let Some(existing) = &mut snapshot.seen_lines {
            existing.extend(lines);
        } else {
            snapshot.seen_lines = Some(lines.clone());
        }
    }

    /// Move retained version history (and read provenance) from `from` to `to`.
    /// No-op when `from` has no history. Used by file moves so tags minted from
    /// reads of the source path stay valid at the destination.
    pub fn relocate(&mut self, from: &Path, to: &Path) {
        let Some(source_versions) = self.by_path.remove(from) else {
            return;
        };
        let relocated: Vec<Snapshot> = source_versions
            .into_iter()
            .map(|mut s| {
                s.path = to.to_path_buf();
                s
            })
            .collect();

        let dest_versions = self.by_path.entry(to.to_path_buf()).or_default();
        // Merge: keep existing versions, append relocated, de-dup by hash+text.
        let mut seen = HashSet::new();
        let mut merged: Vec<Snapshot> = Vec::new();
        for v in dest_versions.drain(..) {
            let key = (v.tag.clone(), v.text.clone());
            if seen.insert(key) {
                merged.push(v);
            }
        }
        for v in relocated {
            let key = (v.tag.clone(), v.text.clone());
            if seen.insert(key) {
                merged.push(v);
            }
        }
        // Keep only the newest MAX_VERSIONS_PER_PATH.
        if merged.len() > MAX_VERSIONS_PER_PATH {
            merged.sort_by_key(|a| a.recorded_at);
            let removed: Vec<_> = merged.drain(..merged.len() - MAX_VERSIONS_PER_PATH).collect();
            for r in &removed {
                self.total_bytes = self.total_bytes.saturating_sub(r.text.len());
            }
        }
        // Re-sort by recorded_at ascending.
        merged.sort_by_key(|a| a.recorded_at);
        *dest_versions = merged;
        self.touch_path(to);
    }

    /// Drop all version history for a single path.
    pub fn invalidate(&mut self, path: &Path) {
        if let Some(versions) = self.by_path.remove(path) {
            for v in &versions {
                self.total_bytes = self.total_bytes.saturating_sub(v.text.len());
            }
        }
        self.path_order.retain(|p| p != path);
    }

    /// Drop all version histories.
    pub fn clear(&mut self) {
        self.by_path.clear();
        self.path_order.clear();
        self.total_bytes = 0;
    }

    fn touch_path(&mut self, path: &Path) {
        self.path_order.retain(|p| p != path);
        self.path_order.push(path.to_path_buf());
        while self.path_order.len() > MAX_PATHS {
            let evicted = self.path_order.remove(0);
            self.invalidate(&evicted);
        }
    }

    /// Evict whole-path histories (LRU order) until total_bytes is under the
    /// global cap. Called after every insertion.
    fn evict_if_over_limit(&mut self) {
        while self.total_bytes > MAX_TOTAL_BYTES && !self.path_order.is_empty() {
            let evicted = self.path_order.remove(0);
            self.invalidate(&evicted);
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

    #[test]
    fn by_content_finds_exact_text() {
        let mut store = SnapshotStore::new();
        store.record(&p("a.rs"), "hello\n");
        let found = store.by_content(&p("a.rs"), "hello\n");
        assert!(found.is_some());
        assert_eq!(found.unwrap().text, "hello\n");
        // Different text should not match.
        assert!(store.by_content(&p("a.rs"), "bye\n").is_none());
    }

    #[test]
    fn find_by_hash_across_paths() {
        let mut store = SnapshotStore::new();
        let s1 = store.record(&p("a.rs"), "x\n");
        let s2 = store.record(&p("b.rs"), "x\n");
        // Same content → same tag.
        assert_eq!(s1.tag, s2.tag);
        let matches = store.find_by_hash(&s1.tag);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn record_seen_lines_merges() {
        let mut store = SnapshotStore::new();
        let snap = store.record(&p("a.rs"), "a\nb\nc\n");
        assert!(snap.seen_lines.is_none());
        let mut lines = HashSet::new();
        lines.insert(1);
        store.record_seen_lines(&p("a.rs"), &snap.tag, &lines);
        let head = store.head(&p("a.rs")).unwrap();
        assert_eq!(head.seen_lines.as_ref().unwrap().len(), 1);
        // Merge more.
        let mut lines2 = HashSet::new();
        lines2.insert(3);
        store.record_seen_lines(&p("a.rs"), &snap.tag, &lines2);
        let head = store.head(&p("a.rs")).unwrap();
        assert_eq!(head.seen_lines.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn relocate_transfers_history() {
        let mut store = SnapshotStore::new();
        let s1 = store.record(&p("old.rs"), "a\n");
        store.record(&p("old.rs"), "b\n");
        store.relocate(&p("old.rs"), &p("new.rs"));
        assert!(store.head(&p("old.rs")).is_none());
        assert!(store.head(&p("new.rs")).is_some());
        // The tag from old should still be findable at new.
        assert!(store.get(&p("new.rs"), &s1.tag).is_some());
    }

    #[test]
    fn total_bytes_tracked() {
        let mut store = SnapshotStore::new();
        store.record(&p("a.rs"), "hello\n");
        assert!(store.total_bytes > 0);
        store.invalidate(&p("a.rs"));
        assert_eq!(store.total_bytes, 0);
    }
}