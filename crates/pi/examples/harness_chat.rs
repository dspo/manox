// Live harness check: AgentHarness over an on-disk session, with the built-in
// tools mounted, runs one prompt against a real Anthropic endpoint (#363
// tools injected, #364 real ToolContext, #372 session persisted). The model
// may call tools; the harness loop executes them and the final transcript is
// printed.
//
// Usage:
//   cargo run -p pi --example harness_chat -- \
//     --base-url https://api.anthropic.com \
//     --api-key sk-ant-... \
//     --model claude-haiku-4-5-20251001 \
//     --prompt "List the files in the current directory, then say done."
//
// --api-key/--base-url/--model fall back to ANTHROPIC_API_KEY /
// ANTHROPIC_BASE_URL / ANTHROPIC_MODEL env vars.

use std::sync::Arc;

use pi::provider::anthropic::AnthropicStreamFn;
use pi::session::Session;
use pi::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use pi::tool::AgentTool;
use pi::tools::bash::BashTool;
use pi::tools::edit::EditTool;
use pi::tools::glob::GlobTool;
use pi::tools::grep::GrepTool;
use pi::tools::ls::LsTool;
use pi::tools::read::ReadTool;
use pi::tools::write::WriteTool;
use pi::types::{ContentBlock, Model, ThinkingKind};
use pi::{AgentHarness, AgentMessage};

struct Args {
    api_key: String,
    base_url: Option<String>,
    model: String,
    prompt: String,
}

fn parse_args() -> Result<Args, String> {
    let mut api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let mut base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
    let mut model = std::env::var("ANTHROPIC_MODEL").ok();
    let mut prompt = None;

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--api-key" => api_key = Some(value),
            "--base-url" => base_url = Some(value),
            "--model" => model = Some(value),
            "--prompt" => prompt = Some(value),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    Ok(Args {
        api_key: api_key
            .filter(|k| !k.is_empty())
            .ok_or("missing --api-key")?,
        base_url: base_url.filter(|u| !u.is_empty()),
        model: model.filter(|m| !m.is_empty()).ok_or("missing --model")?,
        prompt: prompt
            .unwrap_or_else(|| "List the files in the current directory, then say done.".into()),
    })
}

fn default_tools() -> Arc<[Arc<dyn AgentTool>]> {
    Arc::from(vec![
        Arc::new(ReadTool) as Arc<dyn AgentTool>,
        Arc::new(WriteTool),
        Arc::new(EditTool),
        Arc::new(BashTool::new(None)),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
        Arc::new(LsTool),
    ])
}

fn text_of(message: &AgentMessage) -> String {
    let blocks = match message {
        AgentMessage::User { content, .. } | AgentMessage::Assistant { content, .. } => content,
        AgentMessage::ToolResult { content, .. } => content,
        _ => return String::new(),
    };
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let mut stream_fn = AnthropicStreamFn::new(args.api_key);
    if let Some(base) = args.base_url {
        stream_fn = stream_fn.with_base_url(base);
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let meta = JsonlSessionMetadata {
        id: "harness-chat".into(),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into()),
        created_at: chrono::Utc::now(),
        parent_session_path: None,
        metadata: None,
    };
    let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta)
        .await
        .expect("create");
    let session = Session::new(storage);

    let model = Model {
        provider: "anthropic".into(),
        id: args.model.clone(),
        api: "anthropic".into(),
        context_window: 200_000,
        max_tokens: 8_192,
        thinking: ThinkingKind::None,
        metadata: Default::default(),
    };

    let mut harness = AgentHarness::new(
        session,
        "You are a concise agent.",
        model,
        Arc::new(stream_fn),
    )
    .with_tools(default_tools());

    let messages = harness.prompt(&args.prompt).await;
    match messages {
        Ok(messages) => {
            println!("--- transcript ({} messages) ---", messages.len());
            for msg in &messages {
                match msg {
                    AgentMessage::User { .. } => println!("[user] {}", text_of(msg)),
                    AgentMessage::Assistant {
                        stop_reason, usage, ..
                    } => {
                        println!(
                            "[assistant stop={stop_reason:?}] {} (in={} out={})",
                            text_of(msg),
                            usage.input_tokens,
                            usage.output_tokens,
                        );
                    }
                    AgentMessage::ToolResult {
                        tool_name,
                        is_error,
                        ..
                    } => {
                        println!(
                            "[tool_result name={tool_name} error={is_error}] {}",
                            text_of(msg)
                        );
                    }
                    _ => println!("[other]"),
                }
            }
        }
        Err(e) => {
            eprintln!("prompt failed: {e}");
            std::process::exit(1);
        }
    }
}
