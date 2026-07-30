// Drive the harness through a prompt and a compaction, then verify the
// persisted Compaction entry carries a real first-kept entry id, the
// summarization usage, and the retained tail (#374). A mock StreamFn stands
// in for the model so no API key is needed.
//
// Usage:
//   cargo run -p pi --example compact_run

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use pi::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use pi::session::{Session, SessionStorage, SessionTreeEntry};
use pi::types::{AgentEvent, ContentBlock, Model, StopReason, ThinkingKind, Usage};
use pi::{AgentContext, AgentHarness, AgentMessage, StreamFn};

/// A fake model whose every call returns a short assistant turn with a
/// non-zero usage block, so the compaction entry has something to record.
struct SummaryMock;

#[async_trait]
impl StreamFn for SummaryMock {
    async fn stream(
        &self,
        _context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        Ok(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "Compacted: prior turns discussed the hello example.".into(),
                signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 42,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        })
    }
}

fn test_model() -> Model {
    Model {
        provider: "mock".into(),
        id: "mock".into(),
        context_window: 100_000,
        max_tokens: 8_192,
        thinking: ThinkingKind::None,
        metadata: Default::default(),
    }
}

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta = JsonlSessionMetadata {
        id: "demo".into(),
        cwd: dir.path().to_string_lossy().into_owned(),
        created_at: chrono::Utc::now(),
        parent_session_path: None,
        metadata: None,
    };

    let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"), meta)
        .await
        .expect("open");
    let session = Session::new(storage);
    let mut harness = AgentHarness::new(
        session,
        "You are a demo agent.",
        test_model(),
        Arc::new(SummaryMock),
    );

    harness.prompt("hello").await.expect("prompt");
    harness.prompt("world").await.expect("prompt");

    let result = harness.compact().await.expect("compact");
    println!(
        "compacted: tokens_before={} tokens_after={} first_kept_entry_id={:?}",
        result.tokens_before, result.tokens_after, result.first_kept_entry_id
    );

    // Inspect the persisted entry — its fields must match the in-memory result.
    let storage = JsonlSessionStorage::open(
        dir.path(),
        JsonlSessionMetadata {
            id: "demo".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        },
    )
    .await
    .expect("reopen");
    let mut found = None;
    for entry in storage.get_entries().await.expect("entries") {
        if let SessionTreeEntry::Compaction {
            first_kept_entry_id,
            tokens_before,
            usage,
            retained_tail,
            ..
        } = &entry
        {
            found = Some((
                first_kept_entry_id.clone(),
                *tokens_before,
                usage.as_ref().map(|u| u.total_tokens),
                retained_tail.len(),
            ));
        }
    }
    let (id, tb, ut, tail) = found.expect("compaction entry persisted");
    println!(
        "persisted compaction: first_kept_entry_id={id:?} tokens_before={tb} usage_total={ut:?} retained_tail_len={tail}"
    );

    assert_eq!(
        (id, tb, tail),
        (
            result.first_kept_entry_id,
            result.tokens_before,
            harness.agent().state().messages.len().saturating_sub(1)
        )
    );
    assert_eq!(ut, Some(42));
    println!("OK: real entry id, usage, and retained tail survived compaction");
}
