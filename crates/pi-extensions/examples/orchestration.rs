//! End-to-end wiring smoke test: the pi stack — agent loop + `SubagentTool`
//! dispatching the Explore manifest + background orchestration — running as a
//! self-contained agent, no network.
//!
//! Mock `StreamFn`s drive both the main session and the Explore sub-session;
//! the sub-session's `read` tool runs against a real temp file, so the full
//! path (manifest → registry → `SubagentTool` → child session → real tool →
//! collected text) is exercised. Then a background task is spawned through
//! the `BackgroundManager` bound to the session, and the completion event +
//! steered summary are observed.
//!
//! Run: `cargo run -p pi-extensions --example orchestration`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi::agent_loop::{StreamFn, StreamResolver};
use pi::coding_agent::{ModelRuntime, create_agent_session};
use pi::ext_point_agent::AgentRegistry;
use pi::tool::AgentTool;
use pi::types::{AgentEvent, AgentMessage, ContentBlock, Model, StopReason};
use pi_extensions::agents::{SubagentTool, register_defaults};
use pi_extensions::bash::BashTool;
use pi_extensions::bash::orchestration::{BackgroundEvent, BackgroundManager};
use pi_extensions::{BackgroundRegistry, BashOutputTool, TaskStopTool};

/// A stream returning a scripted sequence of assistant messages, one per call.
#[derive(Clone)]
struct Scripted(Arc<Mutex<Vec<AgentMessage>>>);

#[async_trait::async_trait]
impl StreamFn for Scripted {
    async fn stream(
        &self,
        _context: &pi::types::AgentContext,
        _signal: tokio_util::sync::CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        self.0
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("script exhausted"))
    }
}

fn assistant(content: Vec<ContentBlock>) -> AgentMessage {
    AgentMessage::Assistant {
        content,
        model: "mock".into(),
        provider: "mock".into(),
        api: "mock".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        stop_reason: Some(StopReason::Stop),
        raw_stop_reason: None,
        usage: Box::default(),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

fn tool_use(name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: "call_1".into(),
        name: name.into(),
        input,
        thought_signature: None,
    }
}

/// The pi read-only tool set the Explore definition is restricted to.
fn read_only_tools() -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(pi::tools::read::ReadTool),
        Arc::new(pi::tools::grep::GrepTool),
        Arc::new(pi::tools::find::FindTool),
        Arc::new(pi::tools::ls::LsTool),
    ]
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let target = dir.path().join("target.rs");
    std::fs::write(&target, "pub fn wired() {}\n")?;

    // Main session script: first ask Explore, then wrap up.
    let main_script = vec![
        assistant(vec![ContentBlock::Text {
            text: "Finished the delegation.".into(),
            signature: None,
        }]),
        assistant(vec![tool_use(
            "Agent",
            serde_json::json!({
                "subagent_type": "Explore",
                "prompt": "Where is `wired` defined?",
            }),
        )]),
    ];
    let main_stream: Arc<dyn StreamFn> = Arc::new(Scripted(Arc::new(Mutex::new(main_script))));
    let main_resolver: StreamResolver = Arc::new(move |_m: &Model| Ok(Arc::clone(&main_stream)));

    // Explore sub-session script: read the target, then conclude.
    let target_str = target.to_str().unwrap().to_string();
    let sub_script = vec![
        assistant(vec![ContentBlock::Text {
            text: "`wired` is defined in target.rs:1".into(),
            signature: None,
        }]),
        assistant(vec![tool_use(
            "Read",
            serde_json::json!({ "path": target_str }),
        )]),
    ];
    let sub_stream: Arc<dyn StreamFn> = Arc::new(Scripted(Arc::new(Mutex::new(sub_script))));
    let sub_resolver: StreamResolver = Arc::new(move |_m: &Model| Ok(Arc::clone(&sub_stream)));

    // ── Sub-agent dispatch ──────────────────────────────────────────────────
    let mut registry = AgentRegistry::new();
    register_defaults(&mut registry);
    let subagent = SubagentTool::new(Arc::new(registry), read_only_tools())
        .with_model_runtime(ModelRuntime::new(sub_resolver));

    // ── Background orchestration ────────────────────────────────────────────
    let background = Arc::new(BackgroundRegistry::new());
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let mut events = manager.subscribe();
    let bash = BashTool::new(
        Arc::new(pi_extensions::bash::persistent::PersistentShellOperations::new(dir.path())),
        background.clone(),
    )
    .with_manager(Arc::clone(&manager));

    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(subagent),
        Arc::new(bash),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(TaskStopTool::new(background.clone())),
    ];

    let mut session = create_agent_session()
        .with_cwd(dir.path())
        .with_session_dir(dir.path().join(".pi-session"))
        .with_model_runtime(ModelRuntime::new(main_resolver))
        .with_tools(tools)
        .build()
        .await?;

    // Bind the orchestrator to the session: completions steer into it.
    manager.attach(&mut session);

    // 1. Sub-agent closure: the main run delegates to Explore, which reads
    //    the real file and returns its conclusion.
    let messages = session.prompt("Investigate the codebase.").await?;
    // The sub-agent's conclusion lands in the tool_result of the `agent`
    // call; the wrap-up assistant text follows.
    let transcript: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            let content = match m {
                AgentMessage::Assistant { content, .. } => Some(content),
                AgentMessage::ToolResult { content, .. } => Some(content),
                _ => None,
            };
            content.and_then(|blocks| {
                blocks.iter().find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
            })
        })
        .collect();
    let joined = transcript.join("\n");
    assert!(
        joined.contains("wired"),
        "sub-agent conclusion must reach the main transcript: {joined}"
    );
    println!("[1/2] sub-agent dispatch closed: Explore returned its conclusion");

    // 2. Background orchestration: spawn a task; the completion event and the
    //    steered summary (into the session's steering queue) follow.
    let id = manager.spawn("sleep 0.2; echo done", dir.path())?;
    println!("[2/2] spawned background task {id}");

    let mut saw_spawned = false;
    let mut saw_completed = false;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Ok(BackgroundEvent::Spawned { .. })) => saw_spawned = true,
            Ok(Ok(BackgroundEvent::Completed { .. })) => {
                saw_completed = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(saw_spawned, "spawned event observed");
    assert!(saw_completed, "completed event observed");
    println!("[2/2] background orchestration closed: completion event observed");

    let steered = session.steering_messages();
    assert!(
        steered.iter().any(|m| format!("{m:?}").contains(&id.0)),
        "completion summary steered into the session: {steered:?}"
    );
    println!("[2/2] completion summary steered into the session");

    println!("wiring smoke test passed");
    Ok(())
}
