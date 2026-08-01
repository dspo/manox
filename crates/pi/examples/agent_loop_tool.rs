// Local end-to-end check of the agent loop with a built-in tool.
//
// A mock StreamFn plays the model: turn 1 emits a `read` tool call, the loop
// executes it through the real `LocalToolContext` (no panic — #364), and
// turn 2 emits a final answer derived from the tool result. No API key is
// needed; the point is to prove tools are injected into the context (#363)
// and reach the filesystem (#364).
//
// Usage:
//   cargo run -p pi --example agent_loop_tool

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use pi::env::{ExecutionEnv, TokioExecutionEnv};
use pi::tool::{AgentTool, LocalToolContext, ToolContext, ToolState};
use pi::tools::read::ReadTool;
use pi::types::{AgentEvent, ContentBlock, Model, StopReason, ThinkingKind, Usage};
use pi::{Agent, AgentContext, AgentMessage, StreamFn};

/// A fake model that calls `read` once and then summarizes the result.
struct ToolLoopMock {
    step: AtomicU32,
    file_path: String,
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
                model: "mock".into(),
                provider: "mock".into(),
                api: "mock".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason,
                raw_stop_reason: None,
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            };

        match step {
            0 => Ok(assistant(
                vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": self.file_path }),
                    thought_signature: None,
                }],
                Some(StopReason::ToolUse),
            )),
            1 => {
                // The loop has appended the tool result to the context; pull
                // its text out for the final answer.
                let tool_text = context
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        AgentMessage::ToolResult { content, .. } => {
                            content.iter().find_map(|b| match b {
                                ContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| "(no tool result)".into());
                Ok(assistant(
                    vec![ContentBlock::Text {
                        text: format!("The file contains: {tool_text}"),
                        signature: None,
                    }],
                    Some(StopReason::Stop),
                ))
            }
            _ => anyhow::bail!("mock: unexpected extra turn {step}"),
        }
    }
}

#[tokio::main]
async fn main() {
    // Write a tempfile the `read` tool will open.
    let dir = tempfile::tempdir().expect("tempdir");
    let file_path = dir.path().join("hello.txt");
    std::fs::write(&file_path, "hello from disk\n").expect("write");

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let env: Arc<dyn ExecutionEnv> = Arc::new(TokioExecutionEnv::new(cwd.clone()));
    let tool_state = Arc::new(ToolState::new());
    let tool_ctx: Arc<dyn ToolContext> = Arc::new(LocalToolContext::new(env, cwd, tool_state));

    let model = Model {
        provider: "mock".into(),
        id: "mock".into(),
        api: "test".into(),
        context_window: 100_000,
        max_tokens: 8_192,
        thinking: ThinkingKind::None,
        metadata: Default::default(),
    };

    let stream_fn: Arc<dyn StreamFn> = Arc::new(ToolLoopMock {
        step: AtomicU32::new(0),
        file_path: file_path.to_string_lossy().into_owned(),
    });

    let tools: Arc<[Arc<dyn AgentTool>]> =
        Arc::from(vec![Arc::new(ReadTool) as Arc<dyn AgentTool>]);

    let mut agent = Agent::new(
        "You are a tool-using test agent.",
        model,
        Arc::clone(&stream_fn),
        tool_ctx,
    )
    .with_tools(tools);
    let messages = agent
        .prompt("Read hello.txt and tell me what it says.")
        .await
        .expect("prompt");

    println!("turn produced {} messages:", messages.len());
    for msg in &messages {
        match msg {
            AgentMessage::Assistant {
                content,
                stop_reason,
                ..
            } => {
                println!("  assistant stop={stop_reason:?}: {content:?}");
            }
            AgentMessage::ToolResult {
                tool_name,
                is_error,
                content,
                ..
            } => {
                println!("  tool_result name={tool_name} error={is_error}: {content:?}");
            }
            AgentMessage::User { content, .. } => {
                println!("  user: {content:?}");
            }
            _ => println!("  other"),
        }
    }
}
