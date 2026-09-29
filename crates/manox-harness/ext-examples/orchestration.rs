//! End-to-end wiring smoke test: the pi stack — agent loop + background
//! orchestration — running as a self-contained agent, no network.
//!
//! A background task is spawned through the `BackgroundManager` bound to
//! the session, and the completion event + steered summary are observed.
//!
//! Run: `cargo run -p pi-extensions --example orchestration`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use manox_harness::agent_loop::{StreamFn, StreamResolver};
use manox_harness::coding_agent::{ModelRuntime, create_agent_session};
use manox_harness::tool::AgentTool;
use manox_harness::types::{AgentEvent, AgentMessage, ContentBlock, Model, StopReason};
use manox_harness::bash::BashTool;
use manox_harness::bash::orchestration::{BackgroundEvent, BackgroundManager, OutputShape};
use manox_harness::{BackgroundRegistry, BashOutputTool, TaskStopTool};

/// A stream returning a scripted sequence of assistant messages, one per call.
#[derive(Clone)]
struct Scripted(Arc<Mutex<Vec<AgentMessage>>>);

#[async_trait::async_trait]
impl StreamFn for Scripted {
    async fn stream(
        &self,
        _context: &manox_harness::types::AgentContext,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;

    // Main session script: just wrap up.
    let main_script = vec![assistant(vec![ContentBlock::Text {
        text: "Finished.".into(),
        signature: None,
    }])];
    let main_stream: Arc<dyn StreamFn> = Arc::new(Scripted(Arc::new(Mutex::new(main_script))));
    let main_resolver: StreamResolver = Arc::new(move |_m: &Model| Ok(Arc::clone(&main_stream)));

    // ── Background orchestration ────────────────────────────────────────────
    let background = Arc::new(BackgroundRegistry::new());
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let mut events = manager.subscribe();
    let bash = BashTool::new(
        Arc::new(manox_harness::bash::persistent::PersistentShellOperations::new(dir.path())),
        background.clone(),
    )
    .with_manager(Arc::clone(&manager));

    let tools: Vec<Arc<dyn AgentTool>> = vec![
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

    // Background orchestration: spawn a task; the completion event and the
    // steered summary (into the session's steering queue) follow.
    let id = manager.spawn("sleep 0.2; echo done", dir.path(), OutputShape::default())?;
    println!("spawned background task {id}");

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
    println!("background orchestration closed: completion event observed");

    let steered = session.steering_messages();
    assert!(
        steered.iter().any(|m| format!("{m:?}").contains(&id.0)),
        "completion summary steered into the session: {steered:?}"
    );
    println!("completion summary steered into the session");

    println!("wiring smoke test passed");
    Ok(())
}
