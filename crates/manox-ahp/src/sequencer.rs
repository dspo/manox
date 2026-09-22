//! The global `serverSeq` sequencer.
//!
//! AHP sequences the whole server, not each channel: every accepted action gets
//! the next number from one counter, snapshots carry the watermark they were
//! taken at, and `reconnect` compares a single `lastSeenServerSeq`. The counter
//! is the analogue of the v2 journal's single append-point stamp (L4) at the
//! gateway layer; the journal keeps its own dense per-chain `seq`.

use std::sync::atomic::{AtomicI64, Ordering};

/// Monotonic `serverSeq` allocation, shared by every connection of one host.
#[derive(Debug, Default)]
pub struct Sequencer {
    next: AtomicI64,
}

impl Sequencer {
    /// A sequencer resuming after `last` (0 for a fresh host).
    pub fn resuming_after(last: i64) -> Self {
        Self {
            next: AtomicI64::new(last),
        }
    }

    /// Stamp the next action, returning the number it received.
    pub fn stamp(&self) -> i64 {
        self.next.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The highest stamped number so far — the watermark a snapshot carries.
    pub fn watermark(&self) -> i64 {
        self.next.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_dense_and_monotonic() {
        let seq = Sequencer::default();
        assert_eq!(seq.watermark(), 0);
        assert_eq!(seq.stamp(), 1);
        assert_eq!(seq.stamp(), 2);
        assert_eq!(seq.watermark(), 2);
    }

    #[test]
    fn resuming_continues_past_the_given_sequence() {
        let seq = Sequencer::resuming_after(41);
        assert_eq!(seq.stamp(), 42);
    }
}
