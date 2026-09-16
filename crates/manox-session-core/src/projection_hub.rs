//! Per-session projection registry (deepseek-harness session-projection
//! parity): ONE fold per session shared by every follow stream, with a
//! bounded change outbox fanned out to each stream's own cursor.
//!
//! The per-stream fold this replaces let two streams of one session diverge
//! and re-folded the whole chain on every open; here the cell is the
//! authority and a late joiner simply reads its baseline.

use std::collections::{BTreeMap, HashMap, VecDeque};

use manox_agent::thread::ThreadHandle;
use manox_harness::session::SessionTreeEntry;
use serde_json::Value as JsonValue;

use crate::projections::ProjectionSet;

const OUTBOX_CAPACITY: usize = 256;
/// Committed events between durable checkpoints (count trigger; the
/// turn-end and dispose triggers always fire).
const CHECKPOINT_EVERY: u64 = 64;

struct Cell {
    set: ProjectionSet,
    /// Dense-tail seq the fold has consumed (None = seed only).
    observed: Option<u64>,
    outbox: VecDeque<(u64, BTreeMap<String, JsonValue>)>,
    /// Lowest as_of evicted by the outbox cap: a stream still behind it
    /// can no longer be served losslessly and must resync (L5: overflow
    /// is loud, never silent).
    dropped_floor: Option<u64>,
    since_checkpoint: u64,
}

/// [`ProjectionHub::take_since`] outcomes: frames, or a loud "your cursor
/// fell out of the outbox" resync request.
pub enum TakeOutcome {
    Frames(Vec<(u64, BTreeMap<String, JsonValue>)>),
    Stale,
}

#[derive(Default)]
pub struct ProjectionHub {
    cells: std::sync::Mutex<HashMap<String, Cell>>,
}

impl ProjectionHub {
    /// Fold one committed event into the session's cell (deduped by seq);
    /// changed keys land in the outbox as one `(as_of, values)` frame.
    pub fn apply(&self, session_id: &str, seq: u64, entry: &SessionTreeEntry) {
        let mut cells = self.cells.lock().unwrap();
        let Some(cell) = cells.get_mut(session_id) else {
            return;
        };
        if cell.observed.is_some_and(|last| seq <= last) {
            return;
        }
        cell.set.apply_event(seq, entry);
        cell.observed = Some(seq);
        if let Some((as_of, values)) = cell.set.drain_changed() {
            cell.outbox.push_back((as_of, values));
            while cell.outbox.len() > OUTBOX_CAPACITY {
                let (dropped, _) = cell
                    .outbox
                    .pop_front()
                    .expect("outbox non-empty above capacity");
                cell.dropped_floor = Some(dropped);
            }
        }
        cell.since_checkpoint += 1;
        let due = cell.since_checkpoint >= CHECKPOINT_EVERY
            || matches!(
                entry,
                SessionTreeEntry::TurnFinish { .. } | SessionTreeEntry::Stop { .. }
            );
        if due {
            cell.since_checkpoint = 0;
            let rows = cell.set.checkpoint_rows();
            drop(cells);
            crate::projection_cache::save(session_id, &rows);
            return;
        }
        drop(cells);
    }

    /// Outbox frames past one stream's cursor (the fan-out half). A cursor
    /// behind the evicted floor answers [`TakeOutcome::Stale`]: the caller
    /// ends the stream with `Resync` instead of silently missing the last
    /// change of some key (review #805 [sugg] 9).
    pub fn take_since(&self, session_id: &str, last_as_of: Option<u64>) -> TakeOutcome {
        let cells = self.cells.lock().unwrap();
        let Some(cell) = cells.get(session_id) else {
            return TakeOutcome::Frames(Vec::new());
        };
        if cell
            .dropped_floor
            .is_some_and(|floor| last_as_of.is_some_and(|last| last < floor))
        {
            return TakeOutcome::Stale;
        }
        TakeOutcome::Frames(
            cell.outbox
                .iter()
                .filter(|(as_of, _)| last_as_of.is_none_or(|last| *as_of > last))
                .cloned()
                .collect(),
        )
    }

    /// The consistent baseline cut for a snapshot: the cell (restored from
    /// the checkpoint cache when absent and usable, seeded from the live
    /// thread otherwise) advanced over the snapshot's dense records.
    pub fn baseline(
        &self,
        session_id: &str,
        thread: &ThreadHandle,
        records: &[manox_harness::session::jsonl::JournalRecord],
        cursor: u64,
    ) -> (BTreeMap<String, JsonValue>, u64) {
        let mut cells = self.cells.lock().unwrap();
        let cell = cells.entry(session_id.to_string()).or_insert_with(|| {
            let restored = crate::projection_cache::load(session_id, cursor);
            match restored {
                Some((set, observed)) => Cell {
                    set,
                    observed,
                    outbox: VecDeque::new(),
                    dropped_floor: None,
                    since_checkpoint: 0,
                },
                None => Cell {
                    set: ProjectionSet::seed(thread),
                    observed: None,
                    outbox: VecDeque::new(),
                    dropped_floor: None,
                    since_checkpoint: 0,
                },
            }
        });
        // Header facts can move without journal entries on uninteracted
        // threads; reconcile FIRST so the dense record fold (the journal's
        // authority) always wins over the live mirror.
        thread.read(|t| cell.set.reconcile_header(t));
        for record in records {
            if cell.observed.is_some_and(|last| record.seq <= last) {
                continue;
            }
            cell.set.apply_event(record.seq, &record.entry);
            cell.observed = Some(record.seq);
        }
        let _ = cell.set.drain_changed();
        (cell.set.baseline(), cursor)
    }

    /// Replace one session's fold with a fresh live seed (log swap / bind
    /// stub hand-off): the outbox is kept so live streams drain the delta.
    pub fn reseed(&self, session_id: &str, thread: &ThreadHandle) {
        let mut cells = self.cells.lock().unwrap();
        if let Some(cell) = cells.get_mut(session_id) {
            // Fresh live seed; the watermark clears so the next baseline
            // folds the new log's records from the start (correct iff a
            // baseline call follows immediately, which resync guarantees).
            cell.set = crate::projections::ProjectionSet::seed(thread);
            cell.observed = None;
        }
    }

    /// Durable checkpoint + cell drop on session dispose.
    pub fn drop_session(&self, session_id: &str) {
        let mut cells = self.cells.lock().unwrap();
        if let Some(cell) = cells.remove(session_id) {
            let rows = cell.set.checkpoint_rows();
            drop(cells);
            crate::projection_cache::save(session_id, &rows);
        }
    }
}
