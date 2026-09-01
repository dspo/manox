// Drive the harness through two oversized turns and a compaction, then
// verify the persisted Compaction entry carries a real first-kept entry id
// and the summarization usage (#374), and that the session's context
// projection rebuilds the exact post-compaction transcript — the same
// equivalence a restore relies on. A mock StreamFn stands in for the model
// so no API key is needed.
//
// Usage:
//   cargo run -p pi --example compact_run

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use manox_harness::compaction::SUMMARIZATION_SYSTEM_PROMPT;
use manox_harness::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use manox_harness::session::{Session, SessionStorage, SessionTreeEntry};
use manox_harness::types::{AgentEvent, ContentBlock, Model, StopReason, ThinkingKind, Usage};
use manox_harness::{AgentContext, AgentHarness, AgentMessage, StreamFn};

/// A fake model with two voices: conversation turns answer with a long
/// filler text and an escalating usage block, so the transcript outgrows the
/// keep-recent window and the cut lands mid-way; the summarization call —
/// recognized by its system prompt — answers with a short summary and the
/// 42-token usage the compaction entry records.
struct SummaryMock;

fn assistant_message(text: String, total_tokens: u64) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text,
            signature: None,
        }],
        model: "mock".into(),
        provider: "mock".into(),
        api: "mock".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        raw_stop_reason: None,
        stop_reason: Some(StopReason::Stop),
        usage: Box::new(Usage {
            total_tokens,
            ..Default::default()
        }),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

#[async_trait]
impl StreamFn for SummaryMock {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        if context.system_prompt == SUMMARIZATION_SYSTEM_PROMPT {
            return Ok(assistant_message(
                "Compacted: prior turns discussed the hello example.".into(),
                42,
            ));
        }
        // Escalating usage: the latest reply anchors the token estimate.
        let turn = context.messages.len() as u64;
        Ok(assistant_message("x".repeat(2048), 600 * turn))
    }
}

fn test_model() -> Model {
    Model {
        provider: "mock".into(),
        id: "mock".into(),
        api: "test".into(),
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

    let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta)
        .await
        .expect("create");
    let session = Session::new(storage);
    let mut harness = AgentHarness::new(
        session,
        "You are a demo agent.",
        test_model(),
        Arc::new(SummaryMock),
    );

    // Two long exchanges, then a keep-recent window narrower than one
    // exchange: the cut retains only the second assistant reply, so the
    // boundary records that reply's real persisted entry id.
    harness.prompt(&"hello ".repeat(400)).await.expect("prompt");
    harness.prompt(&"world ".repeat(400)).await.expect("prompt");
    harness.set_compaction_settings(manox_harness::compaction::CompactionSettings {
        keep_recent_tokens: 600,
        ..Default::default()
    });

    let result = harness.compact(None).await.expect("compact");
    println!(
        "compacted: tokens_before={} tokens_after={} first_kept_entry_id={:?}",
        result.tokens_before, result.tokens_after, result.first_kept_entry_id
    );
    assert!(
        result.tokens_after < result.tokens_before,
        "compaction shrank the context"
    );

    // Inspect the persisted entry — its fields must match the in-memory result.
    let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
        .await
        .expect("reopen");
    let mut found = None;
    for entry in storage
        .get_entries(Default::default())
        .await
        .expect("entries")
    {
        if let SessionTreeEntry::Compaction {
            first_kept_entry_id,
            tokens_before,
            usage,
            ..
        } = &entry
        {
            found = Some((
                first_kept_entry_id.clone(),
                *tokens_before,
                usage.as_ref().map(|u| u.total_tokens),
            ));
        }
    }
    let (id, tb, ut) = found.expect("compaction entry persisted");
    println!(
        "persisted compaction: first_kept_entry_id={id:?} tokens_before={tb} usage_total={ut:?}"
    );

    // The session's context projection — summary carrier, retained tail, and
    // nothing else — equals the post-compaction transcript exactly, which is
    // what a restore into a fresh harness replays.
    let session = Session::new(storage);
    let projected = session
        .build_session_context()
        .await
        .expect("session context")
        .messages;
    assert_eq!(
        serde_json::to_value(&projected).unwrap(),
        serde_json::to_value(&harness.agent().state().messages).unwrap(),
        "context projection must replay the post-compaction transcript"
    );
    println!("rebuilt transcript: {} messages", projected.len());

    assert_eq!((id, tb), (result.first_kept_entry_id, result.tokens_before));
    assert_eq!(ut, Some(42));
    println!("OK: real entry id, usage, and the rebuilt transcript survived compaction");
}
