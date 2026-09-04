//! [`JournalStream`] — the transport-neutral snapshot-first journal window
//! engine (architecture v2 §F.1, a rule-by-rule port of the deepseek-harness
//! `RemoteJournalStream`, journal-stream.ts:296-373 with the gap-free
//! opening/read semantics of history.ts `follow`).
//!
//! The engine is **pure algebra**: it knows nothing about sessions, the wire,
//! or any async runtime. It folds a stream of [`JournalInput`] events into a
//! gap-free window of entries and emits [`JournalChange`] updates. All domain
//! knowledge is injected:
//!
//! - entries carry an inclusive cursor range `[first, last]` via
//!   [`JournalEntry`] (cursor = `u64` journal sequence, dense and 0-based);
//! - gap repair reads pages through [`JournalSource`] (in production this is
//!   wired to the `PageHistory` call by T4; tests inject a fake);
//! - output flows through the `publish` and `failed` callbacks.
//!
//! Because the cursor is fixed to dense `u64` seq, the dsh pluggable
//! `compare`/`follows` algebra specialises to `u64` ordering and the adjacency
//! test `right == left + 1`. The dsh `emptyCursor` (the cursor of an
//! entry-less journal, `-1` in the signed wire model) is represented by
//! `None` in the engine's internal cursor state (spec §F.1 keeps the u64 wire
//! cursor: the page tail equals it verbatim, while the signed `-1` is only
//! reachable through the Option).
//!
//! # Rules (spec §F.1, each line aligned with dsh)
//!
//! 1. **Opening** ([`JournalInput::Opened`]): the page tail must equal the
//!    opening cursor (dsh `assertPageThrough`) and the page must be
//!    internally adjacent (dsh `assertPage`); publishes `Replace`. A
//!    generation restart (§rule 3) opening behind the last applied cursor is
//!    a protocol violation (`resumed at a cursor behind the last applied
//!    entry`); re-opening at exactly the resume cursor is the seamless
//!    ("无感") case — the old window is replaced by the new snapshot.
//! 2. **Entry** ([`JournalInput::Entry`]): an entry whose `last` is at-or-
//!    behind the window tail is silently dropped (idempotent replay);
//!    `first <= tail < last` is a violation (`partially overlapping entry`);
//!    a hole past the tail is a **gap** — the engine reads repair pages
//!    through [`JournalSource::read_page`] until the hole is sealed, merging
//!    entries that arrive while repairing by ascending `first` (dsh
//!    `mergeReplacement`: stale dropped, partial overlap a violation, a
//!    remaining hole retried once, still short a violation `page did not
//!    reach its opening cursor`) and publishes one `Replace`.
//! 3. **Generation** ([`JournalInput::Generation`]): the connection restarted;
//!    the next [`JournalInput::Opened`] is validated as a resume and the old
//!    window stays published until the new snapshot lands (seamless
//!    reconnect, dsh `restart()` + `replaceGeneration(resumed)`).
//! 4. **Prepend** ([`JournalInput::Prepend`]): a backwards history page; the
//!    page must be internally adjacent and (when it contributes entries) end
//!    immediately before the current window head, otherwise a violation
//!    (`history page is discontinuous`). Entries at-or-after the window head
//!    are dropped (dsh drops entries with `first >= firstCursor` before the
//!    adjacency test, so an already-covered page is a no-op).
//! 5. Cursors (`first`/`last`/`resume`) are recorded on every successful
//!    apply; see [`JournalStream::cursors`] and the
//!    `expectedCursors` field of the shared test vectors.
//!
//! # Degenerated dsh paths (synchronous model)
//!
//! The dsh async machinery — `Promise.race` against the follow iterator, the
//! `superseded` generation hand-off during a page read, and the aborted-page
//! waits — degenerates: [`JournalInput::Generation`] is followed by its
//! [`JournalInput::Opened`] atomically (no interleaved frames exist in the
//! synchronous abstraction), and a repair page that "ended while reading"
//! (dsh `ended while reading its replacement page` /
//! `ended while replacing an aborted page generation`) is exactly a page
//! whose tail falls short of its requested cursor: the engine reports it as
//! `journal ended while reading its replacement page` on the first repair
//! read and `journal page did not reach its opening cursor` after the retry
//! (the shared test vectors pin both).

use std::fmt;

/// One journal entry covering the inclusive cursor range `[first, last]`.
///
/// For wire entries this is the `seq` stamp: single-seq entries have
/// `first == last`; a page packed into one entry (chunk runs) spans its seq
/// interval, which is why the range — not a bare cursor — is the primitive.
pub trait JournalEntry {
    /// Inclusive first durable cursor covered by this entry.
    fn first(&self) -> u64;
    /// Inclusive final cursor; must not precede [`JournalEntry::first`].
    fn last(&self) -> u64;
}

/// One arrival at the engine: an opening snapshot, a live entry, a backwards
/// history page, or a connection-generation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalInput<E> {
    /// A generation's opening frame (the follow stream's `Snapshot`): the
    /// page is the current tail window and `cursor` is the journal cursor at
    /// opening time (the page tail; dsh `opened.cursor`). On a
    /// [`JournalInput::Generation`] re-open `cursor` must not precede the
    /// last applied cursor.
    Opened { cursor: u64, page: Vec<E> },
    /// One live `Entry` frame from the follow stream.
    Entry(E),
    /// A backwards history page (the `PageHistory` result consumed by the
    /// client); `has_more` mirrors the page's "older entries exist" flag
    /// (dsh `options.hasMore(page)`).
    Prepend { page: Vec<E>, has_more: bool },
    /// The connection restarted: the next [`JournalInput::Opened`] belongs to
    /// a new physical generation and is validated as a resume.
    Generation,
}

/// One committed change to the published journal window. The client store
/// (T6/T7) applies these verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalChange<E> {
    /// The whole window was replaced (opening snapshot, or a gap-repair
    /// replacement).
    Replace { entries: Vec<E>, has_more: bool },
    /// Older entries were prepended to the window head.
    Prepend { entries: Vec<E>, has_more: bool },
    /// One entry was appended to the window tail.
    Append(E),
}

/// The addressing source used to seal a gap: reads one journal page whose
/// tail is `through` unless the page is naturally shorter (older history).
///
/// In production this is wired to the `PageHistory` call (T4); tests inject a
/// fake. `name` is the diagnostic stream label embedded in protocol-failure
/// messages (dsh `options.name`).
pub trait JournalSource<E> {
    /// Read the page ending at `through` (inclusive).
    fn read_page(&mut self, through: u64) -> Vec<E>;

    /// Diagnostic stream name used in protocol failures.
    fn name(&self) -> &str {
        "journal"
    }
}

/// Cursor bookkeeping recorded on every successful apply (spec §F.1.4). The
/// shared test vectors pin it via `expectedCursors`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalCursors {
    /// Head (oldest) cursor currently in the published window.
    pub first: Option<u64>,
    /// Tail (newest) cursor currently in the published window (dsh
    /// `lastCursor`).
    pub last: Option<u64>,
    /// Cursor a follow stream must resume from at the next opening (dsh
    /// `resumeCursor`).
    pub resume: Option<u64>,
}

/// The snapshot-first, gap-free journal window engine over a stream of
/// [`JournalInput`] events. See the [module docs](self) for the rule set.
pub struct JournalStream<E> {
    source: Box<dyn JournalSource<E>>,
    publish: Box<dyn FnMut(JournalChange<E>)>,
    failed: Box<dyn FnMut(String)>,
    opened: bool,
    resumed_pending: bool,
    first_cursor: Option<u64>,
    last_cursor: Option<u64>,
    resume_cursor: Option<u64>,
}

impl<E> fmt::Debug for JournalStream<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalStream")
            .field("opened", &self.opened)
            .field("resumed_pending", &self.resumed_pending)
            .field("first_cursor", &self.first_cursor)
            .field("last_cursor", &self.last_cursor)
            .field("resume_cursor", &self.resume_cursor)
            .finish_non_exhaustive()
    }
}

impl<E: JournalEntry + Clone> JournalStream<E> {
    /// Create an engine writing repair pages from `source` and emitting window
    /// changes to `publish` / terminal protocol failures to `failed`.
    pub fn new(
        source: Box<dyn JournalSource<E>>,
        publish: Box<dyn FnMut(JournalChange<E>)>,
        failed: Box<dyn FnMut(String)>,
    ) -> Self {
        Self {
            source,
            publish,
            failed,
            opened: false,
            resumed_pending: false,
            first_cursor: None,
            last_cursor: None,
            resume_cursor: None,
        }
    }

    /// The cursors recorded after the most recent successful apply.
    pub fn cursors(&self) -> JournalCursors {
        JournalCursors {
            first: self.first_cursor,
            last: self.last_cursor,
            resume: self.resume_cursor,
        }
    }

    /// The journal cursor to resume a re-opened follow from, if any.
    pub fn resume_cursor(&self) -> Option<u64> {
        self.resume_cursor
    }

    /// Feed one arrival into the engine.
    ///
    /// Returns `Err(message)` on a protocol violation — the same message is
    /// reported through the `failed` callback before the `Err` is returned,
    /// mirroring dsh (`consume` catch → `options.failed(error)` → throw). A
    /// violation is terminal for the stream in production (the transport
    /// closes it after `failed` fires); `apply` itself is reentrant, so the
    /// shared test vectors can keep driving after a recorded violation.
    pub fn apply(&mut self, input: JournalInput<E>) -> Result<(), String> {
        let result = match input {
            JournalInput::Generation => self.on_generation(),
            JournalInput::Opened { cursor, page } => self.on_opened(cursor, page),
            JournalInput::Entry(entry) => self.on_entry(entry),
            JournalInput::Prepend { page, has_more } => self.on_prepend(page, has_more),
        };
        if let Err(message) = &result {
            (self.failed)(message.clone());
        }
        result
    }

    fn violation<T>(&self, core: &str) -> Result<T, String> {
        Err(format!("{} {core}", self.source.name()))
    }

    fn on_generation(&mut self) -> Result<(), String> {
        if !self.opened {
            return self.violation("generation restart before opening");
        }
        self.resumed_pending = true;
        Ok(())
    }

    fn on_opened(&mut self, cursor: u64, page: Vec<E>) -> Result<(), String> {
        let resumed = std::mem::take(&mut self.resumed_pending);
        if resumed
            && let Some(last) = self.last_cursor
            && cursor < last
        {
            return self.violation("resumed at a cursor behind the last applied entry");
        }
        self.replace_from_opening(cursor, page)?;
        self.opened = true;
        Ok(())
    }

    /// dsh `replaceFromOpening`: `assertPageThrough(page, cursor)` then
    /// `assertPage(entries)` then record `first/last/resume` and publish
    /// `Replace`.
    fn replace_from_opening(&mut self, cursor: u64, page: Vec<E>) -> Result<(), String> {
        // A non-empty opening page must end at its cursor (the dsh
        // `follow` contract). The dsh `emptyCursor` (`-1`, the only cursor an
        // entry-less opening page may carry) has no inclusive `u64` seq value:
        // the u64 encoding of "journal is empty" is an empty page at any
        // cursor, and the tail bookkeeping stays `None` until the first entry
        // or snapshot lands.
        if !page.is_empty() {
            self.assert_page_through(&page, cursor)?;
        }
        self.assert_page(&page)?;
        let has_more = page.first().is_some_and(|entry| entry.first() > 0);
        self.first_cursor = page.first().map(JournalEntry::first);
        self.last_cursor = (!page.is_empty()).then_some(cursor);
        self.set_resume_cursor(cursor);
        (self.publish)(JournalChange::Replace {
            entries: page,
            has_more,
        });
        Ok(())
    }

    fn on_entry(&mut self, entry: E) -> Result<(), String> {
        if !self.opened {
            return self.violation("emitted an entry before its opening cursor");
        }
        let (first, entry_last) = self.entry_range(&entry)?;
        let Some(tail) = self.last_cursor else {
            // dsh: `follows(emptyCursor, first)` ⇒ a fresh journal accepts
            // only the contiguous head entry; anything past it is a gap.
            if first != 0 {
                return self.replace_through(entry_last, vec![entry], false);
            }
            self.first_cursor = Some(first);
            self.last_cursor = Some(entry_last);
            self.set_resume_cursor(entry_last);
            (self.publish)(JournalChange::Append(entry));
            return Ok(());
        };
        if entry_last <= tail {
            // Stale replay: idempotent, silently dropped.
            return Ok(());
        }
        if first <= tail {
            return self.violation("emitted a partially overlapping entry");
        }
        if tail + 1 != first {
            // Adjacency hole: repair through the entry's cursor.
            return self.replace_through(entry_last, vec![entry], false);
        }
        if self.first_cursor.is_none() {
            self.first_cursor = Some(first);
        }
        self.last_cursor = Some(entry_last);
        self.set_resume_cursor(entry_last);
        (self.publish)(JournalChange::Append(entry));
        Ok(())
    }

    /// dsh `replaceThrough`: read a repair page ending at `required`, merge
    /// the entries that queued during the read, retry once when the merged
    /// window still does not reach the target cursor, then publish `Replace`.
    ///
    /// The published window is exactly the repair page plus the queued entries
    /// — the caller is responsible for serving a window-aligned page (dsh's
    /// repair request is derived from the initial page request unbounded, so
    /// `entries(page)` already spans the window; T4 wires `PageHistory` with
    /// the client's retained window head).
    ///
    /// `repaired` tracks the dsh retry position (the second `assertPageThrough`
    /// in dsh reports "page did not reach its opening cursor" only after the
    /// merged window still falls short).
    fn replace_through(
        &mut self,
        required: u64,
        queued: Vec<E>,
        repaired: bool,
    ) -> Result<(), String> {
        let page = self.source.read_page(required);
        if page.is_empty() {
            // The synchronous degradation of dsh's "stream ended while the
            // replacement page was being read" (see module docs).
            return self.violation(if repaired {
                "page did not reach its opening cursor"
            } else {
                "ended while reading its replacement page"
            });
        }
        self.assert_page_through(&page, required)?;
        let merged = self.merge_replacement(&page, &queued)?;
        let target = self.max_cursor(required, &queued);
        let tail = merged
            .as_ref()
            .and_then(|entries| entries.last())
            .map(JournalEntry::last);
        if merged.is_none() || tail.unwrap_or(0) < target {
            return self.replace_through(target, queued, true);
        }
        let entries = merged.expect("checked above");
        let has_more = entries.first().is_some_and(|entry| entry.first() > 0);
        let tail = entries.last().map(JournalEntry::last).unwrap_or(target);
        self.first_cursor = entries.first().map(JournalEntry::first);
        self.last_cursor = Some(tail);
        self.set_resume_cursor(tail);
        (self.publish)(JournalChange::Replace { entries, has_more });
        Ok(())
    }

    /// dsh `mergeReplacement`: validate the repair page, then absorb queued
    /// entries by ascending `first`: stale entries are dropped, a partially
    /// overlapping queued entry is a violation, and a queue entry that does
    /// not adjoin the merged tail leaves a hole (`Ok(None)` → retry the read
    /// with a higher target).
    fn merge_replacement(&self, page: &[E], queued: &[E]) -> Result<Option<Vec<E>>, String> {
        let mut entries: Vec<E> = page.to_vec();
        self.assert_page(&entries)?;
        let mut sorted = queued.to_vec();
        sorted.sort_by_key(JournalEntry::first);
        let mut tail = match entries.last() {
            Some(entry) => entry.last(),
            None => return Ok(Some(entries)),
        };
        for entry in sorted {
            let (first, last) = self.entry_range(&entry)?;
            if last <= tail {
                continue;
            }
            if first <= tail {
                return self.violation("replacement contains a partially overlapping entry");
            }
            if tail + 1 != first {
                return Ok(None);
            }
            tail = last;
            entries.push(entry);
        }
        Ok(Some(entries))
    }

    /// dsh `prepend`: read-and-apply is split on the client; the page arrives
    /// as a [`JournalInput::Prepend`] input.
    fn on_prepend(&mut self, page: Vec<E>, has_more: bool) -> Result<(), String> {
        if !self.opened {
            return self.violation("is not open");
        }
        self.assert_page(&page)?;
        let before = self.first_cursor;
        let accepted: Vec<E> = match before {
            Some(before) => page
                .into_iter()
                .filter(|entry| entry.first() < before)
                .collect(),
            None => page,
        };
        if let (Some(before), Some(tail_entry)) = (before, accepted.last())
            && self.entry_range(tail_entry)?.1 + 1 != before
        {
            (self.publish)(JournalChange::Prepend {
                entries: Vec::new(),
                has_more: false,
            });
            return self.violation("history page is discontinuous");
        }
        if let Some(head) = accepted.first() {
            self.first_cursor = Some(head.first());
        }
        (self.publish)(JournalChange::Prepend {
            entries: accepted,
            has_more,
        });
        Ok(())
    }

    /// dsh `assertPage`: entries are internally adjacent
    /// (`last + 1 == next first`).
    fn assert_page(&self, entries: &[E]) -> Result<(), String> {
        let mut previous: Option<u64> = None;
        for entry in entries {
            let range = self.entry_range(entry)?;
            if let Some(last) = previous
                && last + 1 != range.0
            {
                return self.violation("page contains discontinuous entries");
            }
            previous = Some(range.1);
        }
        Ok(())
    }

    /// dsh `entryRange`: reject an inverted cursor range.
    fn entry_range(&self, entry: &E) -> Result<(u64, u64), String> {
        let first = entry.first();
        let last = entry.last();
        if first > last {
            return self.violation("entry has an inverted cursor range");
        }
        Ok((first, last))
    }

    /// dsh `assertPageThrough`: a non-empty page tail must equal its requested
    /// cursor. An empty page tails at the dsh `emptyCursor`, which has no
    /// valid positive seq-space value — the [`JournalStream::replace_from_opening`]
    /// / [`JournalStream::replace_through`] callers gate emptiness before.
    fn assert_page_through(&self, page: &[E], through: u64) -> Result<(), String> {
        if page.is_empty() || page.last().map(JournalEntry::last) != Some(through) {
            return self.violation("page did not end at its requested cursor");
        }
        Ok(())
    }

    fn max_cursor(&self, cursor: u64, entries: &[E]) -> u64 {
        let mut result = cursor;
        for entry in entries {
            let candidate = entry.last();
            if candidate > result {
                result = candidate;
            }
        }
        result
    }

    fn set_resume_cursor(&mut self, cursor: u64) {
        self.resume_cursor = Some(cursor);
    }
}
