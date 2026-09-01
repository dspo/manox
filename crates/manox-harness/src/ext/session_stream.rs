//! Display-only lazy transcript reader (streaming history preview).
//!
//! The authoritative transcript loader parses a session file eagerly — correct
//! for the restore path, but a large file (long tool outputs, many turns)
//! delays the first rendered message. This module owns a tolerant lazy reader
//! over the same jsonl format: `open` parses only the header, `next_batch`
//! yields entries in append order as the file is read. The stream is
//! display-only — it never feeds the kernel's session/agent state; the
//! authoritative restore replaces the preview at `Ready`. Tolerant by design:
//! a malformed line simply ends the preview, while the authoritative path
//! reports the real error.

use std::path::Path;

use crate::core::session::SessionTreeEntry;
use tokio::io::{AsyncBufReadExt, BufReader};

/// A lazily-read session transcript, oldest entry first.
pub struct SessionTranscriptStream {
    reader: BufReader<tokio::fs::File>,
    /// EOF or the first malformed line reached; `next_batch` returns `None`.
    done: bool,
}

impl SessionTranscriptStream {
    /// Open a session file, consuming only its header line. Content is parsed
    /// on demand; a missing/unreadable file surfaces the error here.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let file = tokio::fs::File::open(path).await?;
        let mut reader = BufReader::new(file);
        // Consume the header line so `next_batch` starts at the first entry.
        // A blank header is tolerated (the authoritative path rejects it); a
        // headerless file simply yields no entries.
        let mut first = String::new();
        let _ = reader.read_line(&mut first).await?;
        Ok(Self {
            reader,
            done: false,
        })
    }

    /// Yield the next batch of entries, oldest first, capped by entry count
    /// and a soft byte budget (an entry larger than the budget is yielded
    /// alone rather than dropped or split). `None` at EOF or after the first
    /// malformed line — the preview stops and the authoritative path owns the
    /// error.
    pub async fn next_batch(
        &mut self,
        max_entries: usize,
        max_bytes: usize,
    ) -> Option<Vec<SessionTreeEntry>> {
        if self.done {
            return None;
        }
        let mut batch: Vec<SessionTreeEntry> = Vec::new();
        let mut bytes = 0usize;
        while batch.len() < max_entries && bytes < max_bytes {
            let mut line = String::new();
            match self.reader.read_line(&mut line).await {
                // 0 bytes read = EOF.
                Ok(0) | Err(_) => {
                    self.done = true;
                    break;
                }
                Ok(_) => {}
            }
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionTreeEntry>(&line) {
                Ok(entry) => {
                    bytes = bytes.saturating_add(line.len());
                    batch.push(entry);
                }
                Err(_) => {
                    self.done = true;
                    break;
                }
            }
        }
        if batch.is_empty() {
            self.done = true;
            None
        } else {
            Some(batch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::SessionTreeEntry;
    use crate::core::types::{AgentMessage, ContentBlock};

    fn message_entry(id: &str, text: &str) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.to_string(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                    signature: None,
                }],
                timestamp: chrono::Utc::now(),
            },
        }
    }

    fn write_session(path: &std::path::Path, header: &str, entries: &[&str]) {
        let mut content = format!("{header}\n");
        for e in entries {
            content.push_str(e);
            content.push('\n');
        }
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn streams_entries_in_append_order_across_batches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let entries: Vec<String> = (0..10)
            .map(|ix| {
                serde_json::to_string(&message_entry(&format!("m{ix}"), &format!("msg-{ix}")))
                    .unwrap()
            })
            .collect();
        write_session(
            &path,
            r#"{"type":"session","version":3,"id":"s1"}"#,
            &entries.iter().map(String::as_str).collect::<Vec<_>>(),
        );

        let mut stream = SessionTranscriptStream::open(&path).await.unwrap();
        let first = stream.next_batch(4, usize::MAX).await.unwrap();
        assert_eq!(first.len(), 4);
        let second = stream.next_batch(4, usize::MAX).await.unwrap();
        assert_eq!(second.len(), 4);
        let third = stream.next_batch(4, usize::MAX).await.unwrap();
        assert_eq!(third.len(), 2);
        assert!(stream.next_batch(4, usize::MAX).await.is_none());
    }

    #[tokio::test]
    async fn byte_budget_yields_oversized_entry_alone_then_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // Entry m0 is large enough to blow the 64-byte budget by itself; the
        // remaining entries are small. The budget must still apply across
        // subsequent batches.
        let entries: Vec<String> = (0..5)
            .map(|ix| {
                let text = if ix == 0 {
                    "x".repeat(200)
                } else {
                    format!("msg-{ix}")
                };
                serde_json::to_string(&message_entry(&format!("m{ix}"), &text)).unwrap()
            })
            .collect();
        write_session(
            &path,
            r#"{"type":"session","version":3,"id":"s1"}"#,
            &entries.iter().map(String::as_str).collect::<Vec<_>>(),
        );

        let mut stream = SessionTranscriptStream::open(&path).await.unwrap();
        let first = stream.next_batch(usize::MAX, 64).await.unwrap();
        assert_eq!(
            first.len(),
            1,
            "oversized entry is yielded alone, not dropped"
        );
        assert_eq!(
            first[0].id(),
            "m0",
            "the oversized entry comes first, preserving append order"
        );
        // The remaining small entries arrive in order across later batches
        // (the byte budget keeps accumulating, so the split count depends on
        // sizes — what matters is nothing is dropped and order holds).
        let mut seen = first.iter().map(|e| e.id().to_string()).collect::<Vec<_>>();
        while let Some(batch) = stream.next_batch(usize::MAX, 64).await {
            for entry in batch {
                seen.push(entry.id().to_string());
            }
        }
        assert_eq!(seen, vec!["m0", "m1", "m2", "m3", "m4"]);
    }
    #[tokio::test]
    async fn malformed_line_stops_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let good = serde_json::to_string(&message_entry("m0", "hello")).unwrap();
        write_session(
            &path,
            r#"{"type":"session","version":3,"id":"s1"}"#,
            &[&good, "not json at all", &good],
        );
        let mut stream = SessionTranscriptStream::open(&path).await.unwrap();
        let batch = stream.next_batch(usize::MAX, usize::MAX).await.unwrap();
        assert_eq!(batch.len(), 1, "stops at the malformed line");
        assert!(stream.next_batch(usize::MAX, usize::MAX).await.is_none());
    }

    #[tokio::test]
    async fn header_only_session_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        write_session(&path, r#"{"type":"session","version":3,"id":"s1"}"#, &[]);
        let mut stream = SessionTranscriptStream::open(&path).await.unwrap();
        assert!(stream.next_batch(usize::MAX, usize::MAX).await.is_none());
    }

    #[tokio::test]
    async fn open_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-session.jsonl");
        assert!(SessionTranscriptStream::open(&path).await.is_err());
    }
}
