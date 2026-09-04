//! T6 §F.1 — the client-side journal stitching wrapper over the pure
//! [`JournalStream`] engine.
//!
//! [`JournalStream`] (from `manox_protocol::journal_stream`) is synchronous:
//! a gap opens when an `Entry` arrives whose seq is past `tail + 1`, and the
//! engine then calls [`JournalSource::read_page`] *inline* to seal it. On the
//! desktop the page comes from an async `PageHistory` RPC, so the engine can
//! never block on it. This wrapper resolves that by **prefetching the repair
//! page before feeding the gap-causing entry**: it detects the gap from the
//! cursors it tracks, stashes the entry, requests the page, and — once the
//! page has arrived — feeds the entry so the engine's inline `read_page` is
//! served synchronously from the buffered page.
//!
//! The wrapper is gpui-free and pure so it is unit-testable headlessly; the
//! gpui leaf in [`crate::client_store_handle`] drives its async side (issuing
//! the `PageHistory` request, re-opening on `Resync`).
//!
//! Semantics pinned to §F.1:
//! - snapshot (`Opened`) is the required first frame;
//! - stale (`last <= tail`) entries drop silently;
//! - a gap buffers the offending entry + everything that follows until the
//!   repair page lands, then feeds them in arrival order (the engine merges
//!   by seq);
//! - a protocol violation (`failed`) is surfaced as
//!   [`FoldOut::Resync`] so the caller re-opens the stream seamlessly.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use manox_protocol::journal::{JournalWireEntry, JournalWireEvent};
use manox_protocol::journal_stream::{JournalChange, JournalEntry, JournalInput, JournalSource, JournalStream};

/// One journal wire entry boxed as an engine entry. `first`/`last` are both
/// the dense seq (single-seq rows); the range primitive is kept for the
/// packed-run future (§F.1).
#[derive(Debug, Clone)]
pub struct FoldEntry(pub JournalWireEntry);

impl JournalEntry for FoldEntry {
    fn first(&self) -> u64 {
        self.0.seq
    }
    fn last(&self) -> u64 {
        self.0.seq
    }
}

/// A repair page request the caller must fulfil with a `PageHistory` call
/// ending at `through_seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub through_seq: u64,
}

/// A committed window change the store folds into its display.
#[derive(Debug, Clone)]
pub enum WindowChange {
    Replace { entries: Vec<JournalWireEntry>, has_more: bool },
    Prepend { entries: Vec<JournalWireEntry>, has_more: bool },
    Append(JournalWireEntry),
}

/// An output event the gpui leaf reacts to.
#[derive(Debug, Clone)]
pub enum FoldOut {
    /// A window change was committed (`publish` fired).
    Change(WindowChange),
    /// A gap opened: the caller must fetch the page ending at
    /// `through_seq` and deliver it via [`JournalFold::deliver_page`].
    NeedPage(PageRequest),
    /// The stream must be re-opened from a fresh snapshot (a violation, or a
    /// `StreamEnd{Resync}`) — seamless reconnect.
    Resync,
}

/// A synchronous page source backed by a buffer the wrapper fills *before*
/// the engine consumes it. An empty buffer yields an empty page, which the
/// engine treats as a violation; the wrapper guarantees it never happens on
/// the gap path by pre-delivering.
struct BufferedSource {
    name: String,
    buffer: Rc<RefCell<Option<Vec<FoldEntry>>>>,
}

impl JournalSource<FoldEntry> for BufferedSource {
    fn read_page(&mut self, _through: u64) -> Vec<FoldEntry> {
        self.buffer.borrow_mut().take().unwrap_or_default()
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// The stitching state machine. Owns the engine and the buffered repair
/// machinery; emits [`FoldOut`] events for the store/leaf to consume.
pub struct JournalFold {
    engine: Option<JournalStream<FoldEntry>>,
    buffer: Rc<RefCell<Option<Vec<FoldEntry>>>>,
    /// Set while a gap-repair page is in flight: further entries queue.
    repairing: Option<u64>,
    /// Entries that arrived during repair, fed (in order) once the page lands.
    queued: VecDeque<FoldEntry>,
    /// Out events accumulated during the last `apply_*` call.
    out: Rc<RefCell<Vec<FoldOut>>>,
}

impl Default for JournalFold {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalFold {
    pub fn new() -> Self {
        Self {
            engine: None,
            buffer: Rc::new(RefCell::new(None)),
            repairing: None,
            queued: VecDeque::new(),
            out: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Open the stream from a snapshot (`StreamFrame::Snapshot`).
    pub fn snapshot(&mut self, cursor: u64, records: Vec<JournalWireEntry>) -> Vec<FoldOut> {
        self.reset_repair();
        if self.engine.is_none() {
            self.build_engine();
        }
        let page: Vec<FoldEntry> = records.into_iter().map(FoldEntry).collect();
        self.take(|engine| engine.apply(JournalInput::Opened { cursor, page }))
    }

    /// Feed one live `Entry` frame.
    pub fn entry(&mut self, seq: u64, event: JournalWireEvent) -> Vec<FoldOut> {
        let entry = FoldEntry(JournalWireEntry {
            seq,
            id: format!("e-{seq}"),
            parent_id: None,
            timestamp: String::new(),
            event,
        });
        // While repairing, everything queues; the repair flush feeds them in
        // arrival order (engine merges by seq).
        if self.repairing.is_some() {
            self.queued.push_back(entry);
            return Vec::new();
        }
        let engine = match self.engine.as_mut() {
            Some(e) => e,
            None => {
                // Entry before a snapshot — a violation surfaced as a resync
                // request (the caller re-opens and gets a proper snapshot).
                return vec![FoldOut::Resync];
            }
        };
        // Gap detection mirrors the engine's rule 2: a hole past the tail
        // needs a repair page (async) before the entry may be fed.
        let tail = engine.cursors().last;
        let first = entry.first();
        if let Some(tail) = tail {
            if first > tail + 1 {
                self.repairing = Some(entry.last());
                let through = entry.last();
                self.queued.push_back(entry);
                return vec![FoldOut::NeedPage(PageRequest { through_seq: through })];
            }
        }
        self.take(|engine| engine.apply(JournalInput::Entry(entry)))
    }

    /// Deliver a previously requested repair page (the `PageHistory` result).
    /// Feeds the buffered entries so the engine's inline `read_page` is
    /// served from the buffer.
    pub fn deliver_page(&mut self, records: Vec<JournalWireEntry>) -> Vec<FoldOut> {
        if self.repairing.is_none() {
            // A stray page with no repair in flight: ignore (the snapshot
            // path already covers it).
            return Vec::new();
        }
        *self.buffer.borrow_mut() = Some(records.into_iter().map(FoldEntry).collect());
        // Feed the queued entries in order; the first triggers the repair
        // read (served from the buffer), later ones merge normally.
        let mut outs = Vec::new();
        while let Some(entry) = self.queued.pop_front() {
            let r = match self.engine.as_mut() {
                Some(engine) => engine.apply(JournalInput::Entry(entry)),
                None => Err("engine gone".to_string()),
            };
            if let Err(ref _v) = r {
                // A failure during repair already fired the engine's `failed`
                // callback; stop and let the caller resync.
                outs.push(FoldOut::Resync);
                break;
            }
            outs.append(&mut self.drain_changes());
        }
        self.reset_repair();
        // The engine drained the buffer into Replace/Append changes via the
        // publish callback captured in `outs`' drain; collect trailing.
        outs.extend(self.drain_changes());
        if r_failed(&outs) {
            return vec![FoldOut::Resync];
        }
        outs
    }

    /// Record a connection generation change (the transport re-opened). The
    /// next snapshot resumes seamlessly.
    pub fn generation(&mut self) -> Vec<FoldOut> {
        self.reset_repair();
        self.take(|engine| engine.apply(JournalInput::Generation))
    }

    /// Feed a backwards history page (the `Prepend` path).
    pub fn prepend_page(
        &mut self,
        records: Vec<JournalWireEntry>,
        has_more: bool,
    ) -> Vec<FoldOut> {
        let page: Vec<FoldEntry> = records.into_iter().map(FoldEntry).collect();
        self.take(|engine| engine.apply(JournalInput::Prepend { page, has_more }))
    }

    /// The window tail cursor (highest applied seq), if any.
    pub fn tail(&self) -> Option<u64> {
        self.engine.as_ref().and_then(|e| e.cursors().last)
    }

    /// A repair is pending (the caller must not feed further entries out of
    /// band; they queue).
    pub fn repairing(&self) -> bool {
        self.repairing.is_some()
    }

    fn build_engine(&mut self) {
        let buffer = Rc::clone(&self.buffer);
        let out = Rc::clone(&self.out);
        let source = Box::new(BufferedSource {
            name: "desktop-follow".to_string(),
            buffer,
        });
        let publish_out = Rc::clone(&out);
        let failed_out = Rc::clone(&out);
        let engine = JournalStream::new(
            source,
            Box::new(move |change| {
                if let Some(out) = change_to_foldout(change) {
                    publish_out.borrow_mut().push(out);
                }
            }),
            Box::new(move |_message| {
                // A protocol violation is terminal for the window: request a
                // seamless re-open.
                failed_out.borrow_mut().push(FoldOut::Resync);
            }),
        );
        self.engine = Some(engine);
    }

    fn reset_repair(&mut self) {
        self.repairing = None;
        self.queued.clear();
    }

    fn take(
        &mut self,
        f: impl FnOnce(&mut JournalStream<FoldEntry>) -> Result<(), String>,
    ) -> Vec<FoldOut> {
        let engine = match self.engine.as_mut() {
            Some(e) => e,
            None => return Vec::new(),
        };
        let _ = f(engine);
        let outs = self.drain_changes();
        if outs.iter().any(|o| matches!(o, FoldOut::Resync)) {
            return vec![FoldOut::Resync];
        }
        outs
    }

    fn drain_changes(&self) -> Vec<FoldOut> {
        std::mem::take(&mut *self.out.borrow_mut())
    }
}

fn change_to_foldout(change: JournalChange<FoldEntry>) -> Option<FoldOut> {
    let mapped = |entries: Vec<FoldEntry>| entries.into_iter().map(|e| e.0).collect();
    Some(match change {
        JournalChange::Replace { entries, has_more } => {
            FoldOut::Change(WindowChange::Replace {
                entries: mapped(entries),
                has_more,
            })
        }
        JournalChange::Prepend { entries, has_more } => {
            FoldOut::Change(WindowChange::Prepend {
                entries: mapped(entries),
                has_more,
            })
        }
        JournalChange::Append(entry) => FoldOut::Change(WindowChange::Append(entry.0)),
    })
}

fn r_failed(outs: &[FoldOut]) -> bool {
    outs.iter().any(|o| matches!(o, FoldOut::Resync))
}

#[cfg(test)]
mod tests {
    use super::*;
    use manox_protocol::journal::JournalWireEvent as E;

    fn entry(seq: u64) -> E {
        E::AgentTextDelta { s: format!("tok-{seq}") }
    }

    #[test]
    fn snapshot_then_append() {
        let mut fold = JournalFold::new();
        let snap = fold.snapshot(0, vec![JournalWireEntry { seq: 0, id: "a".into(), parent_id: None, timestamp: String::new(), event: entry(0) }]);
        assert!(matches!(
            snap.as_slice(),
            [FoldOut::Change(WindowChange::Replace { entries, .. })] if entries.len() == 1
        ));
        let outs = fold.entry(1, entry(1));
        assert!(matches!(
            outs.as_slice(),
            [FoldOut::Change(WindowChange::Append(_))]
        ));
        assert_eq!(fold.tail(), Some(1));
    }

    #[test]
    fn stale_entry_drops_silently() {
        let mut fold = JournalFold::new();
        fold.snapshot(0, vec![JournalWireEntry { seq: 0, id: "a".into(), parent_id: None, timestamp: String::new(), event: entry(0) }]);
        let outs = fold.entry(0, entry(0));
        assert!(outs.is_empty(), "stale replay must not emit a change");
    }

    #[test]
    fn gap_requests_page_then_converges() {
        let mut fold = JournalFold::new();
        fold.snapshot(0, vec![JournalWireEntry { seq: 0, id: "a".into(), parent_id: None, timestamp: String::new(), event: entry(0) }]);
        // seq 2 skips 1 → gap, repair page must end at through=2.
        let outs = fold.entry(2, entry(2));
        assert!(matches!(outs.as_slice(), [FoldOut::NeedPage(PageRequest { through_seq: 2 })]));
        assert!(fold.repairing());
        // seq 3 arrives during repair: it queues (fed after the page).
        let outs2 = fold.entry(3, entry(3));
        assert!(outs2.is_empty());
        // Deliver the repair page ending at seq 2; the engine merges the
        // queued entry (seq 2 already inside the page) and publishes a
        // Replace, then seq 3 appends contiguously.
        let page = vec![1u64, 2].into_iter().map(|s| JournalWireEntry { seq: s, id: format!("e{s}"), parent_id: None, timestamp: String::new(), event: entry(s) }).collect();
        let outs3 = fold.deliver_page(page);
        assert!(
            outs3.iter().any(|o| matches!(
                o,
                FoldOut::Change(WindowChange::Replace { entries, .. })
                    if entries.last().map(|e| e.seq) == Some(2)
            )),
            "expected a repair Replace ending at 2, got {outs3:?}"
        );
        assert!(matches!(
            outs3.iter().last(),
            Some(FoldOut::Change(WindowChange::Append(_)))
        ));
        assert_eq!(fold.tail(), Some(3));
        assert!(!fold.repairing());
    }

    #[test]
    fn resync_requested_when_entry_precedes_snapshot() {
        let mut fold = JournalFold::new();
        let outs = fold.entry(0, entry(0));
        assert!(matches!(outs.as_slice(), [FoldOut::Resync]));
    }
}
