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
// durable journal alone. Each registration rides an activation generation:
// one run id can host more than one activation over time, and a stale
// drive's removal — carrying its own generation — strips nothing of a
// newer activation's faces.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::broadcast;

use crate::core::harness::HarnessHandle;
use crate::core::session::jsonl::JournalEvent;
use crate::sandbox::AccessMode;
use crate::session::Session;
use crate::session::jsonl::JsonlSessionStorage;
use crate::types::AgentMessage;

/// The activation generation a run's faces register under. One run id can
/// host more than one activation over time (a forgotten run whose drive
/// later recovers alongside a continuation reusing the id), so removals
/// carry the generation their registration minted and a stale drive's exit
/// strips nothing of a newer activation's.
pub type Activation = u64;

/// Mint one activation generation. Called once per spawn; the token rides
/// all three registrations and the drive task's removal guard.
pub fn mint_activation() -> Activation {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

type Feeds = RwLock<HashMap<String, (broadcast::Sender<JournalEvent>, Activation)>>;

/// The shared mid-run append face of one child session — the same Arc the
/// drive loop's listener appends through.
type ChildWriter = Arc<Session<JsonlSessionStorage>>;

/// One run's writer entry: the append face, whether the run's mode is
/// declaratively pinned (a pinned run's effective mode cannot change, so a
/// parent mid-turn flip is a no-op for its journal — the opening row
/// already declares the only mode it will ever run under), and the
/// activation generation the entry registered under.
struct ChildWriterEntry {
    writer: ChildWriter,
    pinned: Option<AccessMode>,
    activation: Activation,
}

type Writers = RwLock<HashMap<String, ChildWriterEntry>>;

/// One run's steer entry: the child's decoupled handle, cloned at dispatch,
/// paired with its activation generation.
type Steers = RwLock<HashMap<String, (HarnessHandle, Activation)>>;

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
/// instant a `run/started` row lands still finds the feed. The entry rides
/// the given activation generation (see [`mint_activation`]).
pub fn register_child_feed(
    run_id: &str,
    feed: &broadcast::Sender<JournalEvent>,
    activation: Activation,
) {
    feeds()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(run_id.to_string(), (feed.clone(), activation));
}

/// Register a run's child journal writer (the read half's twin): the append
/// face the host's mode propagation lands rows through, plus the run's mode
/// pin (`Some` when a declarative `mode: sandbox` fixed the child's mode —
/// such a run's journal takes no flip rows). Same dispatch-time
/// registration, same settle-time removal, same activation generation.
pub fn register_child_writer(
    run_id: &str,
    writer: ChildWriter,
    pinned: Option<AccessMode>,
    activation: Activation,
) {
    writers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            run_id.to_string(),
            ChildWriterEntry {
                writer,
                pinned,
                activation,
            },
        );
}

/// Drop a run's registry entry (the drive loop's exit path, every ending
/// included), scoped to the entry registered under `activation`: a NEWER
/// activation's entry (a continuation reusing the run id) survives a stale
/// drive's exit. A bridge already holding a receiver keeps receiving until
/// the child session's sender closes.
pub fn remove_child_feed(run_id: &str, activation: Activation) {
    let mut feeds = feeds()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if feeds
        .get(run_id)
        .is_some_and(|(_, registered)| *registered == activation)
    {
        feeds.remove(run_id);
    }
}

/// Drop a run's writer entry, scoped to its activation generation: a
/// settled journal takes no propagation rows, and a newer activation's
/// writer survives a stale drive's exit.
pub fn remove_child_writer(run_id: &str, activation: Activation) {
    let mut writers = writers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if writers
        .get(run_id)
        .is_some_and(|entry| entry.activation == activation)
    {
        writers.remove(run_id);
    }
}

/// Register a run's steer face: the child's `HarnessHandle`, cloned at
/// dispatch. Same lifecycle as the feed and writer halves — the drive task
/// holds the guard, so the entry dies on every ending, scoped to its
/// activation generation.
pub fn register_child_steer(run_id: &str, handle: &HarnessHandle, activation: Activation) {
    steers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(run_id.to_string(), (handle.clone(), activation));
}

/// Drop a run's steer entry, scoped to its activation generation: a settled
/// child has no live turn to receive a guidance message, and a newer
/// activation's face survives a stale drive's exit.
pub fn remove_child_steer(run_id: &str, activation: Activation) {
    let mut steers = steers()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if steers
        .get(run_id)
        .is_some_and(|(_, registered)| *registered == activation)
    {
        steers.remove(run_id);
    }
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
        .map(|(handle, _)| {
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
        .map(|(feed, _)| feed.subscribe())
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
        let activation = mint_activation();
        register_child_feed(&run_id, &tx, activation);

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
        remove_child_feed(&run_id, activation);
        assert!(child_journal_feed(&run_id).is_none());
        drop(tx);
        assert!(matches!(
            first.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
    }

    /// A stale drive's exit removes nothing of a newer activation's: the
    /// removal carries the generation its registration minted, and only a
    /// matching generation strips the entry. One run id can host more than
    /// one activation (a continuation reusing the id while a recovered old
    /// drive is still exiting) — the old guard's drop must leave the new
    /// faces in place.
    #[test]
    fn an_old_activations_drop_leaves_a_newer_activations_faces() {
        let (old_tx, _) = broadcast::channel(8);
        let run_id = format!("gen-{}", uuid::Uuid::new_v4());
        let old = mint_activation();
        register_child_feed(&run_id, &old_tx, old);
        assert!(child_journal_feed(&run_id).is_some());

        // The run id is re-registered by a newer activation before the old
        // drive's guard drops.
        let (new_tx, _) = broadcast::channel(8);
        let fresh = mint_activation();
        assert_ne!(old, fresh, "generations never repeat");
        register_child_feed(&run_id, &new_tx, fresh);

        remove_child_feed(&run_id, old);
        assert!(
            child_journal_feed(&run_id).is_some(),
            "the newer activation's feed survives the stale drop"
        );

        // The new drive's own exit removes its own registration.
        remove_child_feed(&run_id, fresh);
        assert!(child_journal_feed(&run_id).is_none());
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
        let activation = mint_activation();
        register_child_writer(
            &run_id,
            Arc::new(crate::session::Session::new(storage)),
            None,
            activation,
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

        remove_child_writer(&run_id, activation);
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
            mint_activation(),
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
