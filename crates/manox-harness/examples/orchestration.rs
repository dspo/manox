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
use manox_harness::bash::BashTool;
use manox_harness::bash::orchestration::{BackgroundManager, OutputShape};
use manox_harness::coding_agent::{ModelRuntime, create_agent_session};
use manox_harness::tasks::{Settlement, TaskObserver};
use manox_harness::tool::AgentTool;
use manox_harness::types::{AgentEvent, AgentMessage, ContentBlock, Model, StopReason};
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

/// A minimal lifecycle observer: prints what the producers emit and counts
/// the spawn notifications.
struct PrintObserver {
    spawned: std::sync::atomic::AtomicUsize,
}

impl PrintObserver {
    fn spawned_count(&self) -> usize {
        self.spawned.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl TaskObserver for PrintObserver {
    fn on_spawned(
        &self,
        id: &str,
        family: manox_harness::tasks::TaskFamily,
        label: &str,
        _stop: manox_harness::tasks::StopHandle,
    ) {
        self.spawned
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        println!("spawned {id} ({family:?}): {label}");
    }

    fn on_output(&self, id: &str, line: String) {
        println!("[{id}] {line}");
    }

    fn on_settled(&self, id: &str, settlement: &Settlement) {
        println!(
            "[{id}] settled: {:?} (cause: {:?})",
            settlement.kind, settlement.cause
        );
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
    let manager = BackgroundManager::new(Arc::clone(&background));
    let observer = Arc::new(PrintObserver {
        spawned: std::sync::atomic::AtomicUsize::new(0),
    });
    manager.set_observer(Arc::clone(&observer) as Arc<dyn TaskObserver>);
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

    // Background orchestration: spawn a task; lifecycle emissions print via
    // the observer and the completion summary steers into the session.
    let id = manager.spawn("sleep 0.2; echo done", dir.path(), OutputShape::default())?;
    println!("spawned background task {id}");

    // The spawned notification landed, and the task reached its terminal
    // state (public status read; the example addresses the manager, not the
    // registry).
    let mut saw_completed = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !manager.status(&id, 0).unwrap().is_running {
            saw_completed = true;
            break;
        }
    }
    assert!(saw_completed, "background task reached its terminal state");
    assert!(
        observer.spawned_count() > 0,
        "the Spawned lifecycle notification reached the observer"
    );
    println!("background orchestration closed: task settled");

    let steered = session.steering_messages();
    assert!(
        steered.iter().any(|m| format!("{m:?}").contains(&id.0)),
        "completion summary steered into the session: {steered:?}"
    );
    println!("completion summary steered into the session");

    println!("wiring smoke test passed");
    Ok(())
}
