// The child-session live feed registry.
//
// A running sub-agent's child session journals to its own file, but the
// session object lives inside the spawn provider's drive loop — the AHP
// backend has no thread handle for it the way it does for user sessions.
// The registry bridges the two worlds: the spawn registers the child's
// journal broadcast under the run id at dispatch, and the backend's live
// bridge clones the receiver when a subscriber attaches to the child chat.
// The entry dies with the run (the drive loop removes it at settle; the
// storage's sender drop closes any bridge still parked on the channel), so
// a settled child serves from its durable journal alone, as before.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use tokio::sync::broadcast;

use crate::core::session::jsonl::JournalEvent;

type Feeds = RwLock<HashMap<String, broadcast::Sender<JournalEvent>>>;

fn feeds() -> &'static Feeds {
    static FEEDS: OnceLock<Feeds> = OnceLock::new();
    FEEDS.get_or_init(|| RwLock::new(HashMap::new()))
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

/// Drop a run's registry entry (the drive loop's exit path, every ending
/// included). A bridge already holding a receiver keeps receiving until the
/// child session's sender closes.
pub fn remove_child_feed(run_id: &str) {
    feeds()
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
