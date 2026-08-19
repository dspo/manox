//! Durable goal event stream persistence.
//!
//! The Goal's authoritative state is the strict fold of its event stream
//! (see `crate::goal::fold_goal_events`); this module only appends events and
//! reads them back. Every append is transactional with the owning thread row,
//! and a batch of events (replace = tombstone + create) commits atomically.

use anyhow::{Context as _, Result};
use rusqlite::{Transaction, params};

use super::ThreadsDatabase;

impl ThreadsDatabase {
    /// Read the thread's goal events after `after_seq`, oldest first, as
    /// `(seq, event_type, data_json)`.
    pub fn goal_events(
        &self,
        thread_id: &str,
        after_seq: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT seq, event_type, data FROM thread_events
                 WHERE thread_id = ?1 AND event_type LIKE 'goal\\_%' ESCAPE '\\'
                 AND seq > ?2 ORDER BY seq ASC",
            )
            .context("prepare goal event query")?;
        let rows = stmt.query_map(params![thread_id, after_seq], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Append one or more `(event_type, data_json)` goal events in a single
    /// transaction. `seq` is per-thread monotonic across all event types, so a
    /// batch is contiguous and strictly ordered.
    pub fn append_goal_events(&self, thread_id: &str, events: &[(&str, &str)]) -> Result<()> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn
            .transaction()
            .context("begin goal event append transaction")?;
        ensure_thread_row(&tx, thread_id)?;
        for (event_type, data) in events {
            let next_seq: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM thread_events WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO thread_events (thread_id, seq, event_type, data)
                 VALUES (?1, ?2, ?3, ?4)",
                params![thread_id, next_seq, event_type, data],
            )
            .context("insert goal event")?;
        }
        tx.commit().context("commit goal event append transaction")
    }
}

/// Seed the `threads` parent row for a Goal write. `thread_events` FKs
/// `threads(id)`, but pi sessions are file-persisted and never upsert
/// `threads`; an id-only stub (metadata stays DEFAULT) keeps those writes
/// self-contained. No production surface lists threads from SQLite (the
/// sidebars scan the session repository); if one ever does, id-only stubs
/// would surface as empty rows — filter them or keep the session repository
/// the list source.
fn ensure_thread_row(tx: &Transaction<'_>, thread_id: &str) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO threads (id) VALUES (?1)",
        params![thread_id],
    )
    .context("ensure threads parent row")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::goal::{
        GOAL_EVENT_VERSION, GoalActor, GoalEvent, GoalEventKind, ThreadGoal, fold_goal_events,
    };

    fn created_event(goal: &ThreadGoal) -> (String, String) {
        (
            "goal_created".to_string(),
            serde_json::to_string(&GoalEvent {
                version: GOAL_EVENT_VERSION,
                kind: GoalEventKind::Created {
                    actor: GoalActor::User,
                    goal: goal.clone(),
                    created_at: goal.created_at,
                },
            })
            .unwrap(),
        )
    }

    #[test]
    fn goal_events_round_trip_in_seq_order() {
        let db = super::super::ThreadsDatabase::open_in_memory_test().unwrap();
        let goal = ThreadGoal::new("t1".into(), "Ship Goal".into(), Some(20), None).unwrap();
        let (created_type, created_data) = created_event(&goal);
        let round = (
            "goal_round".to_string(),
            serde_json::to_string(&GoalEvent {
                version: GOAL_EVENT_VERSION,
                kind: GoalEventKind::Round {
                    goal_id: goal.goal_id.clone(),
                    revision: 1,
                    round: 1,
                    turn_id: "turn-1".into(),
                    tokens_delta: 7,
                    admitted_at: goal.created_at + 10,
                },
            })
            .unwrap(),
        );
        db.append_goal_events(
            "t1",
            &[(&created_type, &created_data), (&round.0, &round.1)],
        )
        .unwrap();
        let events = db.goal_events("t1", 0).unwrap();
        assert_eq!(events.len(), 2);
        let state = fold_goal_events(
            &events
                .iter()
                .map(|(_, t, d)| (t.clone(), d.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let current = state.current.unwrap();
        assert_eq!(current.rounds_started, 1);
        assert_eq!(current.tokens_used, 7);

        // Incremental read after_seq skips already-folded events.
        let after = events[0].0;
        let tail = db.goal_events("t1", after).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].1, "goal_round");
    }

    #[test]
    fn append_is_transactional_with_parent_row() {
        let db = super::super::ThreadsDatabase::open_in_memory_test().unwrap();
        let goal = ThreadGoal::new("pi-1".into(), "Ship Goal".into(), None, None).unwrap();
        let (created_type, created_data) = created_event(&goal);
        db.append_goal_events("pi-1", &[(&created_type, &created_data)])
            .unwrap();
        let events = db.goal_events("pi-1", 0).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn stale_goal_data_fails_the_fold() {
        let db = super::super::ThreadsDatabase::open_in_memory_test().unwrap();
        // Old-shape goal_created payloads (no version/event tag) must fail
        // loudly rather than silently producing an empty fold.
        db.append_goal_events("t1", &[("goal_created", r#"{"objective":"x"}"#)])
            .unwrap();
        let events = db.goal_events("t1", 0).unwrap();
        assert!(
            fold_goal_events(
                &events
                    .iter()
                    .map(|(_, t, d)| (t.clone(), d.clone()))
                    .collect::<Vec<_>>(),
            )
            .is_err()
        );
    }
}
