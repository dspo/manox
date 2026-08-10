//! Event coalescing for monitor output.
//!
//! Raw stdout lines and WebSocket frames arrive at unpredictable rates.  The
//! batcher groups them into bounded batches so the model sees a single steer
//! message per batch instead of a provider round-trip per line.

use std::time::Duration;

/// Maximum bytes for a single batched event payload.
const DEFAULT_MAX_EVENT_BYTES: usize = 4 * 1024;
/// Maximum lines per batch before a flush is forced.
const DEFAULT_MAX_BATCH_SIZE: usize = 20;
/// Wall-clock window before a partial batch is flushed.
const DEFAULT_BATCH_INTERVAL: Duration = Duration::from_millis(300);

/// Bounded line accumulator. A batch flushes when it exceeds the byte cap,
/// reaches the line cap, or the caller's timer fires (`batch_interval`).
/// Lines larger than the byte cap are dropped outright.
#[derive(Default)]
pub struct EventBatcher {
    buffer: Vec<String>,
    batch_bytes: usize,
    max_event_bytes: usize,
    max_batch_size: usize,
    batch_interval: Duration,
}

impl EventBatcher {
    pub fn new() -> Self {
        Self {
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            batch_interval: DEFAULT_BATCH_INTERVAL,
            ..Default::default()
        }
    }

    /// Builder-style overrides.
    pub fn with_max_event_bytes(mut self, v: usize) -> Self {
        self.max_event_bytes = v;
        self
    }
    pub fn with_max_batch_size(mut self, v: usize) -> Self {
        self.max_batch_size = v;
        self
    }

    /// Push a line.  Returns `Some(batch)` when the batch should be flushed.
    /// `None` means the line was buffered (or dropped for exceeding the
    /// per-event cap). When triggered, the flush includes the triggering
    /// line.
    pub fn push(&mut self, line: String) -> Option<Vec<String>> {
        let line_bytes = line.len() + 1; // +1 for the newline we'll append

        // Drop the line when it exceeds the per-event cap.
        if line_bytes > self.max_event_bytes {
            return None;
        }

        self.buffer.push(line);
        self.batch_bytes += line_bytes;

        if self.batch_bytes > self.max_event_bytes || self.buffer.len() >= self.max_batch_size {
            return Some(self.take_batch());
        }

        None
    }

    /// Force-flush the current buffer.
    pub fn flush(&mut self) -> Option<Vec<String>> {
        if self.buffer.is_empty() {
            return None;
        }
        Some(self.take_batch())
    }

    /// The interval after which a partial batch should be flushed by the
    /// caller's timer.
    pub fn batch_interval(&self) -> Duration {
        self.batch_interval
    }

    fn take_batch(&mut self) -> Vec<String> {
        let batch = std::mem::take(&mut self.buffer);
        self.batch_bytes = 0;
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_returns_batch_on_explicit_flush() {
        let mut b = EventBatcher::new();
        assert!(b.push("hello".into()).is_none());
        let batch = b.flush();
        assert_eq!(batch, Some(vec!["hello".into()]));
    }

    #[test]
    fn batch_flushes_at_max_size() {
        let mut b = EventBatcher::new().with_max_batch_size(3);
        assert!(b.push("a".into()).is_none());
        assert!(b.push("b".into()).is_none());
        let batch = b.push("c".to_string());
        assert_eq!(batch, Some(vec!["a".into(), "b".into(), "c".into()]));
        // Buffer is now empty after the forced flush.
        assert!(b.buffer.is_empty());
    }

    #[test]
    fn line_exceeding_max_event_bytes_is_dropped() {
        let mut b = EventBatcher::new().with_max_event_bytes(10);
        let long = "x".repeat(20);
        assert!(b.push(long).is_none());
        assert!(b.buffer.is_empty());
    }

    #[test]
    fn batch_exceeds_max_event_bytes_triggers_immediate_flush() {
        let mut b = EventBatcher::new().with_max_event_bytes(20);
        b.push("a".to_string()); // 2 bytes
        b.push("b".to_string()); // 2 bytes
        // Adding a 16-byte line pushes total to 20, which is at the cap.
        // The batch is flushed, including the triggering line.
        let batch = b.push("1234567890123456".to_string());
        assert_eq!(
            batch,
            Some(vec!["a".into(), "b".into(), "1234567890123456".into()])
        );
        assert!(b.buffer.is_empty());
    }

    #[test]
    fn batch_interval_defaults_to_300ms() {
        let b = EventBatcher::new();
        assert_eq!(b.batch_interval(), Duration::from_millis(300));
    }
}
