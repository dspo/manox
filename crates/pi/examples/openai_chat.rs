// End-to-end smoke check for the OpenAI providers against a live endpoint.
//
// Usage:
//   cargo run -p pi --example openai_chat -- \
//     --base-url https://api.openai.com/v1 \
//     --api-key sk-... \
//     --model gpt-5-mini \
//     --prompt "Say hi in three words" \
//     --shape responses --thinking-kind adaptive --thinking high
//
// Compatible endpoints work the same way, e.g. DashScope compatible mode:
//   --base-url https://dashscope.aliyuncs.com/compatible-mode/v1 \
//   --model deepseek-v4 --thinking-kind enabled --thinking high
//
// --api-key/--base-url/--model fall back to the OPENAI_API_KEY /
// OPENAI_BASE_URL / OPENAI_MODEL env vars. Reading env is the caller's
// choice; the SDK itself never reads env vars.
//
// --shape: completions (default) | responses — the API shape to speak.
// --thinking-kind: none (default) | enabled | adaptive — the model's thinking
//   protocol, mapped onto Model.thinking. On the completions shape "enabled"
//   sends {thinking:{type:"enabled"|"disabled"}} and "adaptive" sends
//   reasoning_effort; on the responses shape both encode as
//   reasoning:{effort}.
// --thinking: off | minimal | low | medium | high | xhigh | max — the harness
//   thinking level. Omitting it sends no thinking fields at all.

use pi::provider::openai::completions::CompletionsStreamFn;
use pi::provider::openai::responses::ResponsesStreamFn;
use pi::types::{ContentBlock, Model, ThinkingKind};
use pi::{AgentContext, AgentEvent, AgentMessage, StreamFn};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Args {
    api_key: String,
    base_url: Option<String>,
    model: String,
    prompt: String,
    shape: String,
    thinking_kind: ThinkingKind,
    thinking_level: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut api_key = std::env::var("OPENAI_API_KEY").ok();
    let mut base_url = std::env::var("OPENAI_BASE_URL").ok();
    let mut model = std::env::var("OPENAI_MODEL").ok();
    let mut prompt = None;
    let mut shape = "completions".to_string();
    let mut thinking_kind = ThinkingKind::None;
    let mut thinking_level = None;

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--api-key" => api_key = Some(value),
            "--base-url" => base_url = Some(value),
            "--model" => model = Some(value),
            "--prompt" => prompt = Some(value),
            "--shape" => {
                if value != "completions" && value != "responses" {
                    return Err(format!("unknown --shape {value:?}"));
                }
                shape = value;
            }
            "--thinking" => thinking_level = Some(value),
            "--thinking-kind" => {
                thinking_kind = match value.as_str() {
                    "none" => ThinkingKind::None,
                    "enabled" => ThinkingKind::Enabled,
                    "adaptive" => ThinkingKind::Adaptive,
                    other => return Err(format!("unknown --thinking-kind {other:?}")),
                };
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    Ok(Args {
        api_key: api_key.filter(|k| !k.is_empty()).ok_or("missing --api-key")?,
        base_url: base_url.filter(|u| !u.is_empty()),
        model: model.filter(|m| !m.is_empty()).ok_or("missing --model")?,
        prompt: prompt.unwrap_or_else(|| "Say hi in three words.".into()),
        shape,
        thinking_kind,
        thinking_level,
    })
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

    let f: Box<dyn StreamFn> = match args.shape.as_str() {
        "responses" => {
            let mut f = ResponsesStreamFn::new(args.api_key);
            if let Some(base) = args.base_url {
                f = f.with_base_url(base);
            }
            Box::new(f)
        }
        _ => {
            let mut f = CompletionsStreamFn::new(args.api_key);
            if let Some(base) = args.base_url {
                f = f.with_base_url(base);
            }
            Box::new(f)
        }
    };

    let context = AgentContext {
        system_prompt: "You are a concise assistant.".into(),
        messages: vec![AgentMessage::user(&args.prompt)],
        tools: Vec::new(),
        model: Model {
            provider: "openai".into(),
            id: args.model.clone(),
            context_window: 200_000,
            thinking: args.thinking_kind,
            metadata: Default::default(),
        },
        thinking_level: args.thinking_level.clone(),
        metadata: Default::default(),
    };

    println!(
        "model={} shape={} kind={:?} level={:?}",
        args.model, args.shape, args.thinking_kind, args.thinking_level
    );
    println!("---");

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let printer = tokio::spawn(async move {
        use std::io::Write;
        // Plain-text bytes already printed per block index — block content
        // only ever grows by appending, so per-block suffixes are safe.
        let mut block_lens: Vec<usize> = Vec::new();
        // Tool inputs captured once resolved, flushed when the message ends.
        let mut tool_inputs: std::collections::HashMap<usize, String> = Default::default();
        let mut dim_open = false;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::MessageStart { .. } => {
                    block_lens.clear();
                    tool_inputs.clear();
                    dim_open = false;
                }
                AgentEvent::MessageUpdate { message } => {
                    let AgentMessage::Assistant { content, .. } = &*message else {
                        continue;
                    };
                    for (i, block) in content.iter().enumerate() {
                        let (dim, text) = match block {
                            ContentBlock::Text { text, .. } => (false, text.as_str()),
                            ContentBlock::Thinking { thinking, .. } => (true, thinking.as_str()),
                            ContentBlock::RedactedThinking { .. } => (true, "[redacted thinking]"),
                            ContentBlock::ToolUse { name, input, .. } => {
                                if i >= block_lens.len() {
                                    if dim_open {
                                        print!("\x1b[0m");
                                        dim_open = false;
                                    }
                                    print!("\n[tool_use {name}] ");
                                    block_lens.push(1);
                                }
                                if !input.is_null() {
                                    tool_inputs.insert(i, input.to_string());
                                }
                                continue;
                            }
                            ContentBlock::Image { .. } => (false, "[image]"),
                        };
                        if i >= block_lens.len() {
                            if dim_open {
                                print!("\x1b[0m");
                            }
                            if i > 0 {
                                println!();
                            }
                            if dim {
                                print!("\x1b[2m");
                            }
                            dim_open = dim;
                            block_lens.push(0);
                        }
                        let done = block_lens[i];
                        if text.len() > done {
                            print!("{}", &text[done..]);
                            block_lens[i] = text.len();
                        }
                    }
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::MessageEnd { .. } => {
                    if dim_open {
                        print!("\x1b[0m");
                        dim_open = false;
                    }
                    let mut tails: Vec<_> = tool_inputs.drain().collect();
                    tails.sort_by_key(|(i, _)| *i);
                    for (_, input) in tails {
                        print!("{input}");
                    }
                    println!();
                }
                _ => {}
            }
        }
    });

    let result = f
        .stream(&context, CancellationToken::new(), tx)
        .await;
    printer.await.unwrap();

    match result {
        Ok(AgentMessage::Assistant {
            stop_reason, usage, ..
        }) => {
            println!("---");
            println!(
                "stop_reason={stop_reason:?} input={} output={} cache_read={} cache_write={}",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
            );
        }
        Ok(_) => println!("unexpected non-assistant message"),
        Err(e) => {
            println!("---");
            println!("stream error: {e}");
            std::process::exit(1);
        }
    }
}
