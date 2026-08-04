// Branch navigation with summarization, offline.
//
// Runs two turns, moves the cursor back to the first turn's reply (appending
// a leaf entry), restores the shorter transcript, generates and persists a
// branch summary for the moved-to path, then reopens the file to show the
// branch and the summary survive.
//
// Usage:
//   cargo run -p pi --example navigate_tree

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use pi::compaction::SUMMARIZATION_SYSTEM_PROMPT;
use pi::session::Session;
use pi::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use pi::session::{SessionStorage, SessionTreeEntry};
use pi::types::{AgentContext, AgentEvent, Model, StopReason, ThinkingKind, Usage};
use pi::{AgentHarness, AgentMessage, NavigateTreeOptions, StreamFn};

/// A mock model that answers plain turns and summarizes branches.
struct MockStream;

#[async_trait]
impl StreamFn for MockStream {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let text = if context.system_prompt == SUMMARIZATION_SYSTEM_PROMPT {
            "branch summary: the early exploration turn".to_string()
        } else {
            format!("answer to: {}", message_text(&context.messages))
        };
        Ok(AgentMessage::Assistant {
            content: vec![pi::types::ContentBlock::Text {
                text,
                signature: None,
            }],
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            api: context.model.api.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            raw_stop_reason: None,
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        })
    }
}

fn message_text(messages: &[AgentMessage]) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            AgentMessage::User { content, .. } => match content.first() {
                Some(pi::types::ContentBlock::Text { text, .. }) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default()
}

fn mock_model() -> Model {
    Model {
        provider: "mock".into(),
        api: "mock".into(),
        id: "mock-1".into(),
        context_window: 200_000,
        max_tokens: 8_192,
        thinking: ThinkingKind::None,
        metadata: Default::default(),
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

    let mut harness = AgentHarness::new(
        session,
        "You are a test assistant.",
        mock_model(),
        Arc::new(MockStream),
    );
    harness.prompt("first exploration").await.expect("prompt 1");
    harness
        .prompt("second continuation")
        .await
        .expect("prompt 2");
    println!(
        "full transcript: {} messages",
        harness.agent().state().messages.len()
    );

    // Navigate back to the first turn's reply.
    let entries = harness
        .session()
        .storage()
        .get_entries(Default::default())
        .await
        .unwrap();
    let first_reply = entries
        .iter()
        .find_map(|e| match e {
            SessionTreeEntry::Message {
                id,
                message: AgentMessage::Assistant { .. },
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .expect("first reply");
    harness
        .navigate_tree_with_options(
            &first_reply,
            NavigateTreeOptions {
                summarize: true,
                ..Default::default()
            },
        )
        .await
        .expect("navigate");
    println!(
        "after navigate: {} messages, branch summary persisted",
        harness.agent().state().messages.len()
    );
    assert_eq!(
        harness.agent().state().messages.len(),
        3,
        "first turn + summary carrier"
    );
    drop(harness);

    // Reopen: the branch (leaf entry) and the branch summary survive.
    let reopened = JsonlSessionStorage::open(&path).await.expect("reopen");
    let storage = reopened;
    let entries = storage.get_entries(Default::default()).await.unwrap();
    let has_leaf = entries
        .iter()
        .any(|e| matches!(e, SessionTreeEntry::Leaf { .. }));
    let has_summary = entries
        .iter()
        .any(|e| matches!(e, SessionTreeEntry::BranchSummary { .. }));
    println!(
        "reopened entries: {} (leaf: {has_leaf}, branch summary: {has_summary})",
        entries.len()
    );
    assert!(has_leaf, "the leaf entry records the navigation");
    assert!(has_summary, "the branch summary was persisted");

    let session = Session::new(storage);
    let branch = session.get_branch().await.expect("branch");
    println!("active branch entries: {}", branch.len());
    println!("OK: navigate_tree moved the cursor, summarized the branch, and survived reopen");
}
