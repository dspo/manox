// Split-turn compaction, offline.
//
// A single oversized tool turn (huge tool-use arguments + huge tool result)
// exceeds the keep-recent budget. The cut lands inside the turn; compaction
// summarizes the history and the discarded turn prefix separately and keeps
// the tool chain out of the retained tail. The boundary is persisted, and a
// reopen restores exactly the compacted transcript.
//
// Usage:
//   cargo run -p pi --example split_turn_compact

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use pi::compaction::{CompactionSettings, NothingToCompact};
use pi::session::Session;
use pi::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use pi::types::{AgentMessage, ContentBlock, StopReason, Usage};
use pi::{AgentContext, AgentEvent, AgentHarness, StreamFn};

/// A summarization-only stream: any non-summarization call fails loudly, and
/// each summary request is counted and answered with a fixed summary.
struct SummaryOnlyStream {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl StreamFn for SummaryOnlyStream {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        assert_eq!(
            context.system_prompt,
            pi::compaction::SUMMARIZATION_SYSTEM_PROMPT,
            "the only provider calls in this example are summarizations"
        );
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "summarized history".into(),
                signature: None,
            }],
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            api: context.model.api.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 5,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        })
    }
}

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let meta = JsonlSessionMetadata {
        id: uuid::Uuid::new_v4().to_string(),
        cwd: std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .to_string_lossy()
            .into_owned(),
        created_at: chrono::Utc::now(),
        parent_session_path: None,
        metadata: None,
    };
    let storage = JsonlSessionStorage::create(&path, meta)
        .await
        .expect("create");
    let session = Session::new(storage);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut harness = AgentHarness::new(
        session,
        "You are a test assistant.",
        pi::types::Model {
            provider: "mock".into(),
            api: "mock".into(),
            id: "mock-1".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: pi::types::ThinkingKind::None,
            metadata: Default::default(),
        },
        Arc::new(SummaryOnlyStream {
            calls: Arc::clone(&calls),
        }),
    );
    harness.set_compaction_settings(CompactionSettings {
        keep_recent_tokens: 20,
        ..Default::default()
    });

    // A transcript whose second turn is oversized: user, tool-use assistant
    // (huge args + 90k usage), huge tool result, tiny final answer.
    let transcript = vec![
        AgentMessage::user("earlier work"),
        AgentMessage::user("large tool turn"),
        AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({ "path": "x".repeat(500) }),
                thought_signature: None,
            }],
            model: "mock".into(),
            provider: "mock".into(),
            api: "mock".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        },
        AgentMessage::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text {
                text: "y".repeat(500),
                signature: None,
            }],
            is_error: false,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        },
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "done".into(),
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
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        },
    ];
    harness.agent_mut().replace_transcript(transcript);
    assert!(
        harness.needs_compaction(),
        "the oversized turn must trigger compaction"
    );

    let tokens_before =
        pi::compaction::estimate_context_tokens(&harness.agent().state().messages).tokens;
    let result = match harness.compact(None).await {
        Ok(r) => r,
        Err(e) if e.downcast_ref::<NothingToCompact>().is_some() => {
            panic!("split-turn compaction must not report nothing to compact")
        }
        Err(e) => panic!("compaction failed: {e:#}"),
    };
    println!(
        "summarization calls: {}",
        calls.load(std::sync::atomic::Ordering::SeqCst)
    );
    println!(
        "tokens: {tokens_before} -> {} (split turn: {})",
        result.tokens_after, result.is_split_turn
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "history + turn prefix"
    );
    assert!(result.is_split_turn);
    assert!(
        result.tokens_after < tokens_before,
        "compaction must shrink the context"
    );
    assert!(
        result.summary.contains("**Turn Context (split turn):**"),
        "merged split summary: {}",
        result.summary
    );
    assert_eq!(result.retained_tail.len(), 1, "only the final answer stays");
    drop(harness);

    // Reopen: the compacted transcript restores identically.
    let reopened = JsonlSessionStorage::open(&path).await.expect("reopen");
    let mut restored = AgentHarness::new(
        Session::new(reopened),
        "You are a test assistant.",
        pi::types::Model {
            provider: "mock".into(),
            api: "mock".into(),
            id: "mock-1".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: pi::types::ThinkingKind::None,
            metadata: Default::default(),
        },
        Arc::new(SummaryOnlyStream {
            calls: Arc::clone(&calls),
        }),
    );
    restored.restore().await.expect("restore");
    let restored_len = restored.agent().state().messages.len();
    println!("restored transcript: {restored_len} messages");
    assert_eq!(
        restored_len,
        result.retained_tail.len() + 1,
        "summary carrier + retained tail"
    );
    println!("OK: split-turn compaction shrank the context and survived reopen");
}
