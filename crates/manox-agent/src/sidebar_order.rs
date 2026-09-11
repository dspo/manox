//! The sidebar's durable order account — the single authority over the order
//! threads and project folders are listed in.
//!
//! The account is a hand-owned artifact: an explicit ordered id list per
//! sidebar partition, plus one ordered list of project folders. Nothing derived
//! from time ever re-sorts it — a thread's row moves only when the user drags
//! it, when it leaves the list (its id is pruned), or when it first appears (a
//! new row enters at the head of its partition). Timestamps reach this module
//! exactly once: to settle the relative order of rows surfacing together for
//! the first time.
//!
//! Every change carries DOM `insertBefore` semantics: the caller names the row
//! to move and the row to move it in front of (omitted = append). Numeric
//! indices are never exchanged, because a prepend or a filtered projection
//! would silently re-point an index mid-flight. A self-anchored or
//! already-in-place move is a no-op that neither mutates nor persists, so a
//! redundant drag leaves the file untouched.
//!
//! Best-effort UI truth: an unreadable file is an empty account (the next
//! successful write rebuilds it), and a failed write warns without propagating
//! — the in-memory order stays correct for the running process.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The partition for threads bound to no registered project — the sidebar's
/// loose "Conversations" list. It owns a thread account like any project; what
/// it lacks is a slot in [`SidebarOrder::groups`], because its section sits
/// below every project folder.
pub const LOOSE: &str = "__loose__";

/// A move request naming a row or an anchor the account does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveInvalid {
    /// The row to move is unaccounted.
    Source(String),
    /// The anchor is unaccounted.
    Anchor(String),
}

impl std::fmt::Display for MoveInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveInvalid::Source(id) => write!(f, "thread {id} is not in the order account"),
            MoveInvalid::Anchor(id) => write!(f, "anchor {id} is not in the order account"),
        }
    }
}

impl std::error::Error for MoveInvalid {}

/// The persisted order of the sidebar: folder order plus one thread account per
/// partition. Head of every list = top of the sidebar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SidebarOrder {
    /// Registered project paths in folder display order. A path absent here has
    /// no committed position yet and renders after every listed folder.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Ordered thread ids per partition key (a registered project path or
    /// [`LOOSE`]).
    #[serde(default)]
    pub accounts: HashMap<String, Vec<String>>,
}

impl SidebarOrder {
    /// Move one thread inside its partition's account. `before = None` appends.
    ///
    /// `Ok(true)` = the account changed and the caller must persist; `Ok(false)`
    /// = a no-op move; `Err` = an unaccounted row or anchor, nothing changed.
    pub fn move_thread(
        &mut self,
        partition: &str,
        thread_id: &str,
        before: Option<&str>,
    ) -> Result<bool, MoveInvalid> {
        let Some(account) = self.accounts.get(partition) else {
            return Err(MoveInvalid::Source(thread_id.to_string()));
        };
        validate(account, thread_id, before)?;
        let mut account = account.clone();
        let changed = insert_before(&mut account, thread_id, before);
        if changed {
            self.accounts.insert(partition.to_string(), account);
        }
        Ok(changed)
    }

    /// Move one project folder inside [`Self::groups`]. Same contract as
    /// [`Self::move_thread`].
    pub fn move_group(&mut self, path: &str, before: Option<&str>) -> Result<bool, MoveInvalid> {
        validate(&self.groups, path, before)?;
        let mut groups = self.groups.clone();
        let changed = insert_before(&mut groups, path, before);
        if changed {
            self.groups = groups;
        }
        Ok(changed)
    }

    /// Move an accounted row to the head of its partition. Pinning uses this: a
    /// pinned row leads its partition, so a pin is an explicit order action
    /// (like a drag) rather than a side effect of activity. Returns whether the
    /// account changed. An unaccounted id is a no-op — the next reconcile
    /// prepends it as a first-seen row anyway.
    pub fn float_to_head(&mut self, partition: &str, thread_id: &str) -> bool {
        let Some(account) = self.accounts.get_mut(partition) else {
            return false;
        };
        if !account.iter().any(|id| id == thread_id)
            || account.first().map(String::as_str) == Some(thread_id)
        {
            return false;
        }
        account.retain(|id| id != thread_id);
        account.insert(0, thread_id.to_string());
        true
    }

    /// The folder order restricted to the still-registered paths, with any
    /// newly registered path appended in registration order. The result is the
    /// partition sequence the sidebar renders — and the account's own
    /// [`Self::groups`] recomputed against live membership, so a folder that
    /// vanished between scans leaves no residue.
    pub fn reconcile_groups(&mut self, registered: &[String]) -> Vec<String> {
        self.groups
            .retain(|path| registered.iter().any(|r| r == path));
        for path in registered {
            if !self.groups.iter().any(|g| g == path) {
                self.groups.push(path.clone());
            }
        }
        self.groups.clone()
    }
}

/// Reject a move naming a row or an anchor the list does not hold. A
/// self-anchored request passes validation — it is a legal no-op.
fn validate(list: &[String], id: &str, before: Option<&str>) -> Result<(), MoveInvalid> {
    if !list.iter().any(|x| x == id) {
        return Err(MoveInvalid::Source(id.to_string()));
    }
    if let Some(anchor) = before
        && !list.iter().any(|x| x == anchor)
    {
        return Err(MoveInvalid::Anchor(anchor.to_string()));
    }
    Ok(())
}

/// DOM `insertBefore` on one id list: move `id` in front of `before`, or to the
/// tail when `before` is `None`. Returns whether the list changed.
fn insert_before(list: &mut Vec<String>, id: &str, before: Option<&str>) -> bool {
    if before == Some(id) {
        return false;
    }
    let mut without: Vec<String> = list.iter().filter(|x| *x != id).cloned().collect();
    let at = match before {
        None => without.len(),
        Some(anchor) => without
            .iter()
            .position(|x| x == anchor)
            .unwrap_or(without.len()),
    };
    without.insert(at, id.to_string());
    let changed = without != *list;
    if changed {
        *list = without;
    }
    changed
}

/// One live row's participation in the ordering.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    /// The thread id — the sidebar row key.
    pub id: &'a str,
    /// The partition key: a registered project path or [`LOOSE`].
    pub partition: &'a str,
    /// Pinned rows form the head band of their partition.
    pub pinned: bool,
    /// Unix seconds of the last human interaction; consulted only for rows
    /// appearing in an account for the first time.
    pub interacted_at: i64,
}

/// Reconcile the accounts against the live row set and return each partition's
/// ids in display order — pinned band first, then account rank. Every `rows` id
/// appears in exactly one returned list, and every returned id is a live row.
///
/// Reconciliation is membership-only, never a re-sort: a stored account keeps
/// its order and drops dead ids, while ids it has never seen are ordered among
/// themselves (newest interaction first, id as the deterministic tie-break) and
/// prepended as a block. An account whose partition holds no live row is
/// dropped entirely, so archiving a folder's threads or unregistering the
/// folder leaves no residue.
pub fn reconcile<'a>(order: &mut SidebarOrder, rows: &[Row<'a>]) -> HashMap<&'a str, Vec<&'a str>> {
    let mut by_partition: HashMap<&'a str, Vec<&Row<'a>>> = HashMap::new();
    for row in rows {
        by_partition.entry(row.partition).or_default().push(row);
    }

    let mut ordered: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
    for (partition, members) in by_partition {
        let stored = order.accounts.get(partition).cloned().unwrap_or_default();
        let live: HashSet<&str> = members.iter().map(|m| m.id).collect();
        let mut kept: Vec<String> = stored
            .into_iter()
            .filter(|id| live.contains(id.as_str()))
            .collect();
        let seen: HashSet<&str> = kept.iter().map(String::as_str).collect();
        let mut fresh: Vec<&Row> = members
            .iter()
            .copied()
            .filter(|m| !seen.contains(m.id))
            .collect();
        // The only timestamp sort here: it settles the relative order of rows
        // that surface together for the first time.
        fresh.sort_by(|a, b| {
            b.interacted_at
                .cmp(&a.interacted_at)
                .then_with(|| a.id.cmp(b.id))
        });
        let prepend: Vec<String> = fresh.into_iter().map(|m| m.id.to_string()).collect();
        kept.splice(0..0, prepend);
        // Writing the account back unconditionally is what prunes the dead ids;
        // `kept` now holds exactly the live ids of this partition.
        order.accounts.insert(partition.to_string(), kept.clone());

        let rank: HashMap<&str, usize> = kept
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        let pinned: HashSet<&'a str> = members.iter().filter(|m| m.pinned).map(|m| m.id).collect();
        let mut display: Vec<&'a str> = members.iter().map(|m| m.id).collect();
        // Pinned band first; inside a band the account rank decides, so a move
        // can never cross the band boundary.
        display.sort_by_key(|id| {
            (
                !pinned.contains(*id),
                rank.get(*id).copied().unwrap_or(usize::MAX),
            )
        });
        ordered.insert(partition, display);
    }
    // An untouched account means its partition holds no live row at all (every
    // thread archived, or the folder unregistered) — `rows` is the complete
    // live set, so the account is dead.
    order
        .accounts
        .retain(|partition, ids| !ids.is_empty() && ordered.contains_key(partition.as_str()));
    ordered
}

#[cfg(any(test, feature = "test-support"))]
static TEST_PATH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Point the account at a scratch file (test-support only — the real state home
/// must never be touched by tests). `None` restores the default path.
#[cfg(any(test, feature = "test-support"))]
pub fn set_order_path_for_test(path: Option<PathBuf>) {
    *TEST_PATH.lock().unwrap() = path;
}

/// The account file under the manox state home.
pub fn order_path() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(path) = TEST_PATH.lock().unwrap().clone() {
        return path;
    }
    crate::paths::manox_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("sidebar.order.json")
}

/// Load the account. A missing file is an empty account; an unreadable one logs
/// and degrades to empty (the next successful write rewrites it).
pub async fn load() -> SidebarOrder {
    let path = order_path();
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "sidebar order unreadable; self-healing on next write");
            SidebarOrder::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SidebarOrder::default(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "sidebar order unreadable; treating as empty");
            SidebarOrder::default()
        }
    }
}

/// Atomic write (temp file + rename) so a crash cannot truncate the account.
pub async fn save(order: &SidebarOrder) -> Result<(), anyhow::Error> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(tokio::sync::Mutex::default).lock().await;
    let path = order_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(order)?;
    // Suffix the full path (`with_extension` would drop the `.json`), so the tmp
    // file lands beside the account and renames over it.
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path override is process-global; this lock serializes the tests that
    /// move it (their guards are held across awaits).
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    /// An account whose folder order is `groups`.
    fn order_with_groups(groups: &[&str]) -> SidebarOrder {
        SidebarOrder {
            groups: ids(groups),
            accounts: HashMap::new(),
        }
    }

    fn order_with(partition: &str, list: &[&str]) -> SidebarOrder {
        let mut order = SidebarOrder::default();
        order.accounts.insert(partition.to_string(), ids(list));
        order
    }

    fn row(
        id: &'static str,
        partition: &'static str,
        pinned: bool,
        interacted_at: i64,
    ) -> Row<'static> {
        Row {
            id,
            partition,
            pinned,
            interacted_at,
        }
    }

    #[test]
    fn move_thread_places_the_row_before_its_anchor() {
        let mut order = order_with("/p/a", &["t1", "t2", "t3"]);
        assert_eq!(order.move_thread("/p/a", "t3", Some("t1")), Ok(true));
        assert_eq!(order.accounts["/p/a"], ids(&["t3", "t1", "t2"]));
    }

    #[test]
    fn move_thread_without_an_anchor_appends() {
        let mut order = order_with("/p/a", &["t1", "t2"]);
        assert_eq!(order.move_thread("/p/a", "t1", None), Ok(true));
        assert_eq!(order.accounts["/p/a"], ids(&["t2", "t1"]));
    }

    #[test]
    fn self_anchored_and_already_in_place_moves_are_silent_noops() {
        let mut order = order_with("/p/a", &["t1", "t2"]);
        assert_eq!(order.move_thread("/p/a", "t1", Some("t1")), Ok(false));
        assert_eq!(order.move_thread("/p/a", "t1", Some("t2")), Ok(false));
        assert_eq!(order.accounts["/p/a"], ids(&["t1", "t2"]));
    }

    #[test]
    fn moves_naming_an_unaccounted_row_or_anchor_are_rejected() {
        let mut order = order_with("/p/a", &["t1"]);
        assert_eq!(
            order.move_thread("/p/a", "ghost", Some("t1")),
            Err(MoveInvalid::Source("ghost".into()))
        );
        assert_eq!(
            order.move_thread("/p/a", "t1", Some("ghost")),
            Err(MoveInvalid::Anchor("ghost".into()))
        );
        assert_eq!(
            order.move_thread("/p/void", "t1", None),
            Err(MoveInvalid::Source("t1".into()))
        );
        assert_eq!(order.accounts["/p/a"], ids(&["t1"]));
    }

    #[test]
    fn move_group_reorders_folders_with_the_same_contract() {
        let mut order = order_with_groups(&["/p/a", "/p/b", "/p/c"]);
        assert_eq!(order.move_group("/p/c", Some("/p/a")), Ok(true));
        assert_eq!(order.groups, ids(&["/p/c", "/p/a", "/p/b"]));
        assert_eq!(order.move_group("/p/c", Some("/p/c")), Ok(false));
        assert_eq!(
            order.move_group("/p/c", Some("/p/a")),
            Ok(false),
            "the folder already sits before its anchor — a no-op move must not write"
        );
        assert_eq!(
            order.move_group("/p/void", None),
            Err(MoveInvalid::Source("/p/void".into()))
        );
        assert_eq!(
            order.move_group("/p/c", Some("/p/void")),
            Err(MoveInvalid::Anchor("/p/void".into()))
        );
    }

    #[test]
    fn reconcile_groups_keeps_registered_order_and_appends_newcomers() {
        let mut order = order_with_groups(&["/p/b", "/p/gone", "/p/a"]);
        let registered = ids(&["/p/a", "/p/b", "/p/c"]);
        assert_eq!(
            order.reconcile_groups(&registered),
            ids(&["/p/b", "/p/a", "/p/c"])
        );
        assert_eq!(order.groups, ids(&["/p/b", "/p/a", "/p/c"]));
    }

    #[test]
    fn reconcile_preserves_a_stored_account_and_drops_dead_ids() {
        let mut order = order_with("/p/a", &["t1", "gone", "t2"]);
        let rows = [row("t2", "/p/a", false, 10), row("t1", "/p/a", false, 20)];
        let ordered = reconcile(&mut order, &rows);
        assert_eq!(order.accounts["/p/a"], ids(&["t1", "t2"]));
        assert_eq!(ordered["/p/a"], vec!["t1", "t2"]);
    }

    #[test]
    fn reconcile_prepends_first_seen_rows_newest_first() {
        let mut order = order_with("/p/a", &["old"]);
        let rows = [
            row("old", "/p/a", false, 1),
            row("new", "/p/a", false, 30),
            row("mid", "/p/a", false, 20),
        ];
        let ordered = reconcile(&mut order, &rows);
        assert_eq!(order.accounts["/p/a"], ids(&["new", "mid", "old"]));
        assert_eq!(ordered["/p/a"], vec!["new", "mid", "old"]);
    }

    #[test]
    fn reconcile_is_deterministic_for_an_equal_stamp() {
        for _ in 0..8 {
            let mut order = SidebarOrder::default();
            let rows = [
                row("bbb", "/p/a", false, 7),
                row("aaa", "/p/a", false, 7),
                row("ccc", "/p/a", false, 7),
            ];
            let ordered = reconcile(&mut order, &rows);
            assert_eq!(order.accounts["/p/a"], ids(&["aaa", "bbb", "ccc"]));
            assert_eq!(ordered["/p/a"], vec!["aaa", "bbb", "ccc"]);
        }
    }

    #[test]
    fn reconcile_floats_pinned_rows_within_their_partition() {
        let mut order = order_with("/p/a", &["t1", "t2", "t3"]);
        let rows = [
            row("t1", "/p/a", false, 1),
            row("t2", "/p/a", true, 2),
            row("t3", "/p/a", false, 3),
        ];
        let ordered = reconcile(&mut order, &rows);
        // The account order still holds inside each band: t2 leads the pinned
        // band, then t1 before t3 by stored rank.
        assert_eq!(ordered["/p/a"], vec!["t2", "t1", "t3"]);
    }

    #[test]
    fn reconcile_partitions_independently() {
        let mut order = SidebarOrder::default();
        let rows = [
            row("t1", "/p/a", false, 1),
            row("t2", LOOSE, false, 2),
            row("t3", "/p/a", false, 3),
        ];
        let ordered = reconcile(&mut order, &rows);
        assert_eq!(order.accounts["/p/a"], ids(&["t3", "t1"]));
        assert_eq!(order.accounts[LOOSE], ids(&["t2"]));
        assert_eq!(ordered[LOOSE], vec!["t2"]);
    }

    #[test]
    fn reconcile_drops_an_account_whose_partition_emptied() {
        let mut order = order_with("/p/a", &["t1"]);
        let rows = [row("t2", "/p/b", false, 5)];
        reconcile(&mut order, &rows);
        assert_eq!(order.accounts.get("/p/a"), None);
        assert_eq!(order.accounts["/p/b"], ids(&["t2"]));
    }

    #[test]
    fn an_empty_live_set_empties_the_accounts() {
        let mut order = order_with("/p/a", &["t1"]);
        order.groups = ids(&["/p/a"]);
        let empty: [Row; 0] = [];
        reconcile(&mut order, &empty);
        assert!(order.accounts.is_empty());
        // Folder order is membership-driven separately: an emptied partition
        // does not unregister the project.
        assert_eq!(order.groups, ids(&["/p/a"]));
    }

    #[test]
    fn order_round_trips_through_json() {
        let mut order = order_with_groups(&["/p/b", "/p/a"]);
        order.accounts.insert("/p/a".into(), ids(&["t1", "t2"]));
        let json = serde_json::to_string(&order).unwrap();
        let back: SidebarOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(back, order);
    }

    #[test]
    fn a_partial_or_absent_file_reads_as_an_empty_account() {
        let bare: SidebarOrder = serde_json::from_str("{}").unwrap();
        assert!(bare.groups.is_empty() && bare.accounts.is_empty());
        let partial: SidebarOrder = serde_json::from_str(r#"{"groups":["/p/a"]}"#).unwrap();
        assert_eq!(partial.groups, ids(&["/p/a"]));
        assert!(partial.accounts.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_round_trip_atomically() {
        let _guard = TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        set_order_path_for_test(Some(dir.path().join("sidebar.order.json")));
        let mut order = order_with_groups(&["/p/a"]);
        order.accounts.insert("/p/a".into(), ids(&["t1", "t2"]));
        save(&order).await.unwrap();
        assert_eq!(load().await, order);
        let residue: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "tmp file left behind: {residue:?}");
        set_order_path_for_test(None);
    }

    #[tokio::test]
    async fn a_missing_file_loads_empty() {
        let _guard = TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        set_order_path_for_test(Some(dir.path().join("absent.json")));
        assert_eq!(load().await, SidebarOrder::default());
        set_order_path_for_test(None);
    }

    #[tokio::test]
    async fn a_corrupt_file_degrades_and_self_heals() {
        let _guard = TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidebar.order.json");
        set_order_path_for_test(Some(path.clone()));
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load().await, SidebarOrder::default());
        let order = order_with_groups(&["/p/a"]);
        save(&order).await.unwrap();
        assert_eq!(load().await, order);
        set_order_path_for_test(None);
    }
}
