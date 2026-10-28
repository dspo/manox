// The child-session live registry.
//
// A running sub-agent's child session journals to its own file, but the
// session object lives inside the spawn provider's drive loop — the AHP
// backend has no thread handle for it the way it does for user sessions.
// The registry bridges the two worlds in both directions: the READ half
// hands the child's journal broadcast to a bridge attaching to the child
// chat, and the WRITE half lets the host land a row in the live child's
// journal (the parent's mid-turn access-mode flip). Both halves register
// at dispatch and die with the run (the drive loop removes them at settle;
// the storage's sender drop closes any bridge still parked on the
// channel), so a settled child serves from its durable journal alone.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::broadcast;

use crate::core::session::jsonl::JournalEvent;
use crate::sandbox::AccessMode;
use crate::session::Session;
use crate::session::jsonl::JsonlSessionStorage;

type Feeds = RwLock<HashMap<String, broadcast::Sender<JournalEvent>>>;

/// The shared mid-run append face of one child session — the same Arc the
/// drive loop's listener appends through.
type ChildWriter = Arc<Session<JsonlSessionStorage>>;

type Writers = RwLock<HashMap<String, ChildWriter>>;

fn feeds() -> &'static Feeds {
    static FEEDS: OnceLock<Feeds> = OnceLock::new();
    FEEDS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn writers() -> &'static Writers {
    static WRITERS: OnceLock<Writers> = OnceLock::new();
    WRITERS.get_or_init(|| RwLock::new(HashMap::new()))
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
/// face the host's mode propagation lands rows through. Same dispatch-time
/// registration, same settle-time removal.
pub fn register_child_writer(run_id: &str, writer: ChildWriter) {
    writers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(run_id.to_string(), writer);
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
/// rows.
pub async fn journal_access_mode(run_id: &str, mode: AccessMode) -> anyhow::Result<bool> {
    let writer = writers()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(run_id)
        .map(Arc::clone);
    let Some(writer) = writer else {
        return Ok(false);
    };
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
        register_child_writer(&run_id, Arc::new(crate::session::Session::new(storage)));

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
}
