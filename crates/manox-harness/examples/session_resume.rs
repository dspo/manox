// Per-message persistence and resume, offline.
//
// Every MessageEnd is appended to the JSONL session immediately (the harness
// persistence middleware), so a session holds each completed message even if
// the process dies mid-turn. This example runs a tool turn, drops the harness
// ("crash"), reopens the file, restores, and continues — the reopened
// transcript matches the persisted prefix exactly.
//
// Usage:
//   cargo run -p pi --example session_resume

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use manox_harness::session::Session;
use manox_harness::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use manox_harness::tool::AgentTool;
use manox_harness::tools::read::ReadTool;
use manox_harness::types::{
    AgentContext, AgentEvent, ContentBlock, Model, StopReason, ThinkingKind, Usage,
};
use manox_harness::{AgentHarness, AgentMessage, StreamFn};

/// A mock model: tool-use turn first, then a plain answer.
struct ToolLoopMock {
    step: AtomicU32,
}

#[async_trait]
impl StreamFn for ToolLoopMock {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let step = self.step.fetch_add(1, Ordering::Relaxed);
        let assistant =
            |content: Vec<ContentBlock>, stop_reason: Option<StopReason>| AgentMessage::Assistant {
                content,
                model: context.model.id.clone(),
                provider: context.model.provider.clone(),
                api: context.model.api.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason,
                raw_stop_reason: None,
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            };
        Ok(if step == 0 {
            assistant(
                vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({ "path": "hello.txt" }),
                    thought_signature: None,
                }],
                Some(StopReason::ToolUse),
            )
        } else {
            assistant(
                vec![ContentBlock::Text {
                    text: "done".into(),
                    signature: None,
                }],
                Some(StopReason::Stop),
            )
        })
    }
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
    // The file the mock's `Read` call targets; tools run against the
    // tempdir via `with_tool_cwd` below.
    std::fs::write(dir.path().join("hello.txt"), "hello from disk\n").expect("write hello.txt");
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
        Arc::new(ToolLoopMock {
            step: AtomicU32::new(0),
        }),
    )
    .with_tools(Arc::from(vec![Arc::new(ReadTool) as Arc<dyn AgentTool>]))
    .with_tool_cwd(dir.path().to_path_buf());

    let messages = harness.prompt("read hello.txt").await.expect("prompt");
    let turn_messages = messages.len();
    println!("turn produced {turn_messages} messages (user, tool-use, tool result, reply)");

    // Regression guard (#430): the mounted `Read` tool must actually execute —
    // a silent "Tool not found" would defeat this example's purpose.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            AgentMessage::ToolResult {
                is_error: false,
                ..
            }
        )),
        "the Read tool must execute without error"
    );

    // Drop the harness without any further writes — a simulated crash. The
    // JSONL file already holds every completed message.
    drop(harness);
    let persisted = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    println!(
        "persisted entries after \"crash\": {persisted} (header + {})",
        persisted - 1
    );
    assert!(
        persisted >= turn_messages,
        "every completed message survived the crash"
    );

    // Reopen and restore: the transcript matches the persisted prefix.
    let reopened = JsonlSessionStorage::open(&path).await.expect("reopen");
    let mut restored = AgentHarness::new(
        Session::new(reopened),
        "You are a test assistant.",
        mock_model(),
        Arc::new(ToolLoopMock {
            step: AtomicU32::new(10),
        }),
    )
    .with_tools(Arc::from(vec![Arc::new(ReadTool) as Arc<dyn AgentTool>]))
    .with_tool_cwd(dir.path().to_path_buf());
    restored.restore().await.expect("restore");
    let transcript_len = restored.agent().state().messages.len();
    println!("restored transcript: {transcript_len} messages");
    assert_eq!(
        transcript_len, turn_messages,
        "restore reproduces the persisted prefix"
    );

    // Continue the session with a fresh prompt.
    let more = restored.prompt("say done").await.expect("continue");
    println!("continued turn produced {} messages", more.len());
    println!("OK: per-message persistence, reopen, restore, and continue all hold");
}
