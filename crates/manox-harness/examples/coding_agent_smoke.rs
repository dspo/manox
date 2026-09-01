// Coding-agent facade smoke test, offline.
//
// create_agent_session → load project instructions + skill/template →
// tool call → model switch → compact → close/reopen → continue. A fake
// provider runtime stands in for the real APIs; the session, tools,
// resources, and compaction all run for real.
//
// Usage:
//   cargo run -p pi --example coding_agent_smoke

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use manox_harness::coding_agent::model_runtime::ModelRuntime;
use manox_harness::coding_agent::{ResourceLoader, create_agent_session};
use manox_harness::types::{
    AgentContext, AgentEvent, ContentBlock, Model, StopReason, ThinkingKind, Usage,
};
use manox_harness::{AgentMessage, StreamFn};

/// A fake provider: tool-use turn first, then answers.
struct FakeProvider {
    step: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl StreamFn for FakeProvider {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
        _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let step = self.step.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let assistant = |content, stop| AgentMessage::Assistant {
            content,
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            api: context.model.api.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(stop),
            raw_stop_reason: None,
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        if step == 0 {
            Ok(assistant(
                vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({ "path": "README.md" }),
                    thought_signature: None,
                }],
                StopReason::ToolUse,
            ))
        } else if step == 1 {
            // The summarization call for compaction.
            Ok(assistant(
                vec![ContentBlock::Text {
                    text: "compacted history".into(),
                    signature: None,
                }],
                StopReason::Stop,
            ))
        } else {
            Ok(assistant(
                vec![ContentBlock::Text {
                    text: format!("done under {}", context.model.id),
                    signature: None,
                }],
                StopReason::Stop,
            ))
        }
    }
}

fn fake_runtime() -> ModelRuntime {
    use manox_harness::coding_agent::model_runtime::ModelCatalog;
    struct MockCatalog;
    impl ModelCatalog for MockCatalog {
        fn resolve(&self, provider: &str, model_id: &str) -> Option<Model> {
            (provider == "mock").then(|| Model {
                provider: "mock".into(),
                api: "mock".into(),
                id: model_id.into(),
                context_window: 100_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            })
        }
    }
    let provider = Arc::new(FakeProvider {
        step: std::sync::atomic::AtomicU32::new(0),
    }) as Arc<dyn StreamFn>;
    let resolver: manox_harness::agent_loop::StreamResolver =
        Arc::new(move |_model: &Model| Ok(Arc::clone(&provider)));
    ModelRuntime::new(resolver).with_catalog(Arc::new(MockCatalog))
}

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    // An isolated agent dir keeps set_model from writing the real
    // ~/.pi/agent/settings.json; the smoke run must not touch the host.
    let agent_dir = dir.path().join("agent");
    tokio::fs::create_dir_all(&agent_dir).await.unwrap();
    // A CLAUDE.md (automatic context), a project skill, and a prompt
    // template in the TS `.pi` layout.
    tokio::fs::write(cwd.join("CLAUDE.md"), "Keep changes minimal.")
        .await
        .unwrap();
    // The file the mock's `Read` call targets during the tool turn.
    tokio::fs::write(cwd.join("README.md"), "# smoke fixture\n")
        .await
        .unwrap();
    tokio::fs::create_dir_all(cwd.join(".pi/skills"))
        .await
        .unwrap();
    tokio::fs::write(
        cwd.join(".pi/skills/review.md"),
        "---\nname: review\ndescription: review the work\n---\nCheck the diff.",
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(cwd.join(".pi/prompts"))
        .await
        .unwrap();
    tokio::fs::write(cwd.join(".pi/prompts/review.md"), "Review {target}.")
        .await
        .unwrap();

    let mut session = create_agent_session()
        .with_cwd(cwd.clone())
        .with_session_dir(dir.path().join("sessions"))
        .with_agent_dir(agent_dir.clone())
        .with_model_runtime(fake_runtime())
        .with_model(Model {
            provider: "mock".into(),
            api: "mock".into(),
            id: "alpha".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        })
        .build()
        .await
        .expect("build");

    // Loaded resources: project instructions became a skill, the template is
    // available.
    let resources = ResourceLoader::new(&cwd).snapshot().await.unwrap();
    assert_eq!(resources.context_files.len(), 1, "CLAUDE.md is context");
    assert_eq!(resources.skills.len(), 1, ".pi/skills loads");
    assert_eq!(resources.prompt_templates.len(), 1, ".pi/prompts loads");
    println!(
        "resources: {} context files, {} skills, {} templates",
        resources.context_files.len(),
        resources.skills.len(),
        resources.prompt_templates.len()
    );

    // Tool turn.
    let messages = session.prompt("read README.md").await.expect("prompt");
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
    println!("tool turn produced {} messages", messages.len());

    // Model switch + compact. A tiny context window guarantees the
    // conversation is over the threshold.
    session
        .set_model(Model {
            provider: "mock".into(),
            api: "mock".into(),
            id: "beta".into(),
            context_window: 200,
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        })
        .await
        .expect("set_model");
    session.set_compaction_settings(manox_harness::compaction::CompactionSettings {
        keep_recent_tokens: 10,
        ..Default::default()
    });
    session.compact(None).await.expect("compact");
    session.prompt("continue the work").await.expect("prompt 2");

    let stats = session.stats().await.expect("stats");
    println!(
        "session stats: messages={} tokens={} cost={:.6}",
        stats.message_count, stats.total_tokens, stats.cost_total
    );

    // Close and reopen: the session resumes.
    let _path = session.close().await.expect("close");
    let repo =
        manox_harness::session::repository::SessionRepository::new(dir.path().join("sessions"));
    let listed = repo.list().await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    let mut resumed = create_agent_session()
        .with_cwd(cwd)
        .with_agent_dir(agent_dir)
        .with_model_runtime(fake_runtime())
        .open(listed[0].path.clone())
        .await
        .expect("reopen");
    // open() restores the session; no manual restore.
    resumed.prompt("resume").await.expect("resume prompt");
    println!(
        "resumed transcript: {} messages",
        resumed.harness_messages().len()
    );
    println!(
        "OK: coding-agent facade — session, tools, resources, model switch, compact, reopen, continue"
    );
}
