// Provider runtime switch across a session, offline.
//
// Two mock providers stand in for different wire APIs: provider A (the
// construction-time stream) plays a tool-use turn, provider B (the queued
// model's api) plays the completing answer. A mid-run `set_model` switches
// the resolver's pick at the next turn boundary; the session persists the
// model change, and after a reopen + restore the next prompt still uses
// provider B.
//
// Usage:
//   cargo run -p pi --example runtime_switch

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use pi::session::Session;
use pi::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use pi::tool::AgentTool;
use pi::tools::read::ReadTool;
use pi::types::{AgentContext, AgentEvent, ContentBlock, Model, StopReason, ThinkingKind, Usage};
use pi::{AgentHarness, AgentMessage, StreamFn};

/// Provider A: a tool-use turn first, then a plain answer. Records the models
/// it served.
struct ProviderA {
    step: AtomicU32,
    served: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl StreamFn for ProviderA {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        self.served.lock().unwrap().push(context.model.id.clone());
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
                    text: "provider A done".into(),
                    signature: None,
                }],
                Some(StopReason::Stop),
            )
        })
    }
}

/// Provider B: a plain answer. Records the models it served.
struct ProviderB {
    served: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl StreamFn for ProviderB {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        self.served.lock().unwrap().push(context.model.id.clone());
        Ok(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "provider B done".into(),
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

fn model_a() -> Model {
    Model {
        provider: "acme".into(),
        api: "acme_completions".into(),
        id: "alpha".into(),
        context_window: 200_000,
        max_tokens: 8_192,
        thinking: ThinkingKind::None,
        metadata: Default::default(),
    }
}

fn model_b() -> Model {
    Model {
        provider: "acme".into(),
        api: "acme_responses".into(),
        id: "beta".into(),
        context_window: 200_000,
        max_tokens: 16_384,
        thinking: ThinkingKind::None,
        metadata: Default::default(),
    }
}

fn build_resolver(
    provider_a: Arc<dyn StreamFn>,
    provider_b: Arc<dyn StreamFn>,
) -> pi::agent_loop::StreamResolver {
    Arc::new(move |model: &Model| {
        if model.api == "acme_responses" {
            Ok(Arc::clone(&provider_b))
        } else {
            Ok(Arc::clone(&provider_a))
        }
    })
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

    let served_a: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let served_b: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let provider_a: Arc<dyn StreamFn> = Arc::new(ProviderA {
        step: AtomicU32::new(0),
        served: Arc::clone(&served_a),
    });
    let provider_b: Arc<dyn StreamFn> = Arc::new(ProviderB {
        served: Arc::clone(&served_b),
    });
    let resolver = build_resolver(Arc::clone(&provider_a), Arc::clone(&provider_b));

    let mut harness = AgentHarness::new(
        session,
        "You are a test assistant.",
        model_a(),
        Arc::clone(&provider_a),
    )
    .with_stream_resolver(resolver)
    .with_tools(Arc::from(vec![Arc::new(ReadTool) as Arc<dyn AgentTool>]))
    .with_tool_cwd(dir.path().to_path_buf());

    // Mid-run switch to provider B on the first TurnEnd.
    let handle = harness.handle();
    let handle_in_listener = handle.clone();
    use std::sync::atomic::AtomicUsize;
    let turn_ends = Arc::new(AtomicUsize::new(0));
    let turns = Arc::clone(&turn_ends);
    let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
        let handle = handle_in_listener.clone();
        let turns = Arc::clone(&turns);
        Box::pin(async move {
            if matches!(event, AgentEvent::TurnEnd { .. })
                && turns.fetch_add(1, Ordering::Relaxed) == 0
            {
                handle.set_model(model_b());
            }
        })
    }));

    let messages = harness.prompt("use the tool").await.expect("prompt");
    println!("served by A: {:?}", *served_a.lock().unwrap());
    println!("served by B: {:?}", *served_b.lock().unwrap());
    assert_eq!(*served_a.lock().unwrap(), vec!["alpha".to_string()]);
    assert_eq!(*served_b.lock().unwrap(), vec!["beta".to_string()]);
    assert!(messages.iter().any(|m| matches!(
        m,
        AgentMessage::Assistant {
            stop_reason: Some(StopReason::Stop),
            content,
            ..
        } if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "provider B done"))
    )));
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
    drop(harness);
    drop(_sub);

    // Reopen the persisted session: the model change survives, and the next
    // prompt still runs under provider B.
    let reopened = JsonlSessionStorage::open(&path).await.expect("reopen");
    let provider_a2: Arc<dyn StreamFn> = Arc::new(ProviderA {
        step: AtomicU32::new(10),
        served: Arc::clone(&served_a),
    });
    let provider_b2: Arc<dyn StreamFn> = Arc::new(ProviderB {
        served: Arc::clone(&served_b),
    });
    let mut restored = AgentHarness::new(
        Session::new(reopened),
        "You are a test assistant.",
        model_a(),
        Arc::clone(&provider_a2),
    )
    .with_stream_resolver(build_resolver(provider_a2, provider_b2))
    .with_model_resolver(|mref: &pi::session::SessionModelRef| {
        (mref.provider == "acme" && mref.model_id == "beta").then(model_b)
    });
    restored.restore().await.expect("restore");
    assert_eq!(restored.model().id, "beta", "restore must keep provider B");

    let messages = restored
        .prompt("hello again")
        .await
        .expect("prompt after restore");
    assert!(messages.iter().any(|m| matches!(
        m,
        AgentMessage::Assistant {
            stop_reason: Some(StopReason::Stop),
            content,
            ..
        } if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "provider B done"))
    )));
    println!(
        "reopened session: next prompt served by B: {:?}",
        *served_b.lock().unwrap()
    );
    println!("OK: provider runtime switched across the session and survived reopen");
}
