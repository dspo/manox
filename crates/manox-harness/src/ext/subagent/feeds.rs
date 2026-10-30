// The child-session live registry.
//
// A running sub-agent's child session journals to its own file, but the
// session object lives inside the spawn provider's drive loop — the AHP
// backend has no thread handle for it the way it does for user sessions.
// The registry bridges the two worlds in three directions: the READ half
// hands the child's journal broadcast to a bridge attaching to the child
// chat, the WRITE half lets the host land a row in the live child's
// journal (the parent's mid-turn access-mode flip), and the STEER half
// reaches the child's `HarnessHandle` so the dispatcher can inject a
// mid-run guidance message through the kernel's intra-session steer
// primitive. Every half registers at dispatch and dies with the run (the
// drive loop removes them at settle; the storage's sender drop closes any
// bridge still parked on the channel), so a settled child serves from its
// durable journal alone.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::broadcast;

use crate::core::harness::HarnessHandle;
use crate::core::session::jsonl::JournalEvent;
use crate::sandbox::AccessMode;
use crate::session::Session;
use crate::session::jsonl::JsonlSessionStorage;
use crate::types::AgentMessage;

type Feeds = RwLock<HashMap<String, broadcast::Sender<JournalEvent>>>;

/// The shared mid-run append face of one child session — the same Arc the
/// drive loop's listener appends through.
type ChildWriter = Arc<Session<JsonlSessionStorage>>;

/// One run's writer entry: the append face plus whether the run's mode is
/// declaratively pinned. A pinned run's effective mode cannot change, so a
/// parent mid-turn flip is a no-op for its journal — the opening row
/// already declares the only mode it will ever run under.
struct ChildWriterEntry {
    writer: ChildWriter,
    pinned: Option<AccessMode>,
}

type Writers = RwLock<HashMap<String, ChildWriterEntry>>;

/// One run's steer entry: the child's decoupled handle, cloned at dispatch.
type Steers = RwLock<HashMap<String, HarnessHandle>>;

fn feeds() -> &'static Feeds {
    static FEEDS: OnceLock<Feeds> = OnceLock::new();
    FEEDS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn writers() -> &'static Writers {
    static WRITERS: OnceLock<Writers> = OnceLock::new();
    WRITERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn steers() -> &'static Steers {
    static STEERS: OnceLock<Steers> = OnceLock::new();
    STEERS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a run's child journal feed. The spawn calls this at dispatch,
/// right after the child session exists — a bridge that attaches the
/// instant a `run/started` row lands still finds the feed.
pub fn register_child_feed(run_id: &str, feed: &broadcast::Sender<JournalEvent>) {
    feeds()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(run_id.to_string(), feed.clone());
}

/// Register a run's child journal writer (the read half's twin): the append
/// face the host's mode propagation lands rows through, plus the run's mode
/// pin (`Some` when a declarative `mode: sandbox` fixed the child's mode —
/// such a run's journal takes no flip rows). Same dispatch-time
/// registration, same settle-time removal.
pub fn register_child_writer(run_id: &str, writer: ChildWriter, pinned: Option<AccessMode>) {
    writers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(run_id.to_string(), ChildWriterEntry { writer, pinned });
}

/// Drop a run's registry entry (the drive loop's exit path, every ending
/// included). A bridge already holding a receiver keeps receiving until the
/// child session's sender closes.
pub fn remove_child_feed(run_id: &str) {
    feeds()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(run_id);
}

/// Drop a run's writer entry: a settled journal takes no propagation rows.
pub fn remove_child_writer(run_id: &str) {
    writers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(run_id);
}

/// Register a run's steer face: the child's `HarnessHandle`, cloned at
/// dispatch. Same lifecycle as the feed and writer halves — the drive task
/// holds the guard, so the entry dies on every ending.
pub fn register_child_steer(run_id: &str, handle: &HarnessHandle) {
    steers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(run_id.to_string(), handle.clone());
}

/// Drop a run's steer entry: a settled child has no live turn to receive a
/// guidance message.
pub fn remove_child_steer(run_id: &str) {
    steers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(run_id);
}

/// Inject one guidance message into a live run's current (or next) turn
/// through the kernel's steer primitive. `false` when the id names no
/// running child — the run settled between the caller's roster read and
/// this call, and a settled run takes no mid-run input.
pub fn steer_child(run_id: &str, message: AgentMessage) -> bool {
    steers()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(run_id)
        .map(|handle| {
            handle.steer(message);
            true
        })
        .unwrap_or(false)
}

/// A receiver for a live run's child journal. `None` when the id names no
/// running child — a settled or unknown run serves from its durable
/// journal. The sender-side registry entry stays for the run's whole life,
/// so a late subscriber's bridge also subscribes.
pub fn child_journal_feed(run_id: &str) -> Option<broadcast::Receiver<JournalEvent>> {
    feeds()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(run_id)
        .map(broadcast::Sender::subscribe)
}

/// Whether a live writer is registered for the run — the test/observability
/// read face of the write half's liveness.
pub fn child_journal_writer(run_id: &str) -> bool {
    writers()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(run_id)
}

/// Land one `access_mode_change` row in a live child's journal — the
/// parent's mid-turn access-mode flip, propagated. The child's enforcement
/// already follows the parent's gate by pointer (live inheritance); the row
/// is its durable witness, carrying the mode now in force. `Ok(false)` when
/// the id names no running child — the run settled between the caller's
/// roster read and this call, and a settled journal takes no propagation
/// rows. A mode-pinned run answers `Ok(true)` without a row: its effective
/// mode cannot change, and its opening declaration already names the only
/// mode it will ever run under — a flip row would be a false change.
pub async fn journal_access_mode(run_id: &str, mode: AccessMode) -> anyhow::Result<bool> {
    let entry = writers()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(run_id)
        .map(|e| (Arc::clone(&e.writer), e.pinned));
    let Some((writer, pinned)) = entry else {
        return Ok(false);
    };
    if pinned.is_some() {
        return Ok(true);
    }
    writer.append_access_mode_change(mode.wire()).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStorage;

    #[test]
    fn a_registered_feed_clones_for_every_subscriber_and_dies_with_the_run() {
        let (tx, _) = broadcast::channel(8);
        let run_id = format!("feed-{}", uuid::Uuid::new_v4());
        register_child_feed(&run_id, &tx);

        let mut first = child_journal_feed(&run_id).expect("the feed is live");
        let mut second = child_journal_feed(&run_id).expect("clones keep coming");
        tx.send(JournalEvent {
            seq: 4,
            entry: std::sync::Arc::new(crate::session::SessionTreeEntry::Title {
                id: "e-4".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                title: "both receivers see it".into(),
            }),
        })
        .unwrap();
        assert!(first.try_recv().is_ok());
        assert!(second.try_recv().is_ok());

        // The run settles: the registry entry goes, a later bridge answers
        // None, and the held receivers see the channel close with the
        // sender.
        remove_child_feed(&run_id);
        assert!(child_journal_feed(&run_id).is_none());
        drop(tx);
        assert!(matches!(
            first.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
    }

    /// The write half: a registered writer lands the propagated row in the
    /// child's own journal, and removal (run settle) closes the path — a
    /// late propagation reads `Ok(false)` instead of writing behind the
    /// run's back.
    #[tokio::test]
    async fn journal_access_mode_lands_on_the_live_child_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let storage = crate::session::jsonl::JsonlSessionStorage::create(
            &path,
            crate::session::jsonl::JsonlSessionMetadata {
                id: "writer-test".into(),
                cwd: dir.path().display().to_string(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: Some(serde_json::json!({})),
            },
        )
        .await
        .expect("storage creates");
        let run_id = format!("writer-{}", uuid::Uuid::new_v4());
        register_child_writer(
            &run_id,
            Arc::new(crate::session::Session::new(storage)),
            None,
        );

        assert!(
            journal_access_mode(&run_id, AccessMode::Sandbox)
                .await
                .expect("the live child takes the row"),
            "a registered writer answers live"
        );
        let entries = crate::session::jsonl::JsonlSessionStorage::open(&path)
            .await
            .unwrap()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            matches!(
                entries.last(),
                Some(crate::session::SessionTreeEntry::AccessModeChange { mode, .. }) if mode == "sandbox"
            ),
            "the propagated row landed in the child journal: {entries:?}"
        );

        remove_child_writer(&run_id);
        assert!(
            !journal_access_mode(&run_id, AccessMode::FullAccess)
                .await
                .expect("the read answers"),
            "a settled child takes no propagation rows"
        );
        let entries = crate::session::jsonl::JsonlSessionStorage::open(&path)
            .await
            .unwrap()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1, "the post-settle flip wrote nothing");
    }

    /// A mode-pinned run stays live for propagation (`Ok(true)`) but lands
    /// no row: its opening declaration already names the only mode it will
    /// ever run under, and a flip row would be a false change.
    #[tokio::test]
    async fn a_mode_pinned_child_answers_live_but_takes_no_flip_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned.jsonl");
        let storage = crate::session::jsonl::JsonlSessionStorage::create(
            &path,
            crate::session::jsonl::JsonlSessionMetadata {
                id: "pinned-writer-test".into(),
                cwd: dir.path().display().to_string(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: Some(serde_json::json!({})),
            },
        )
        .await
        .expect("storage creates");
        let run_id = format!("pinned-{}", uuid::Uuid::new_v4());
        register_child_writer(
            &run_id,
            Arc::new(crate::session::Session::new(storage)),
            Some(AccessMode::Sandbox),
        );

        assert!(
            journal_access_mode(&run_id, AccessMode::FullAccess)
                .await
                .expect("the pinned child answers live"),
            "a pinned child is a live run — handled, not skipped"
        );
        let entries = crate::session::jsonl::JsonlSessionStorage::open(&path)
            .await
            .unwrap()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert_eq!(
            entries.len(),
            0,
            "a pinned run's journal takes no flip rows: {entries:?}"
        );
    }
}
