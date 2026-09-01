// Live integration test for AnthropicStreamFn against the real API.
//
// Gated on ANTHROPIC_API_KEY; skipped (not failed) when the key is absent so
// CI without credentials stays green. Run explicitly with:
//   cargo test -p pi --test anthropic_live -- --ignored --nocapture

use manox_harness::core::provider::ProviderError;
use manox_harness::core::provider::anthropic::AnthropicStreamFn;
use manox_harness::types::{Model, ThinkingKind};
use manox_harness::{AgentContext, AgentEvent, AgentMessage, StreamFn};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn real_model() -> Model {
    Model {
        provider: "anthropic".into(),
        id: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into()),
        api: "anthropic".into(),
        context_window: 200_000,
        max_tokens: 8_192,
        thinking: ThinkingKind::None,
        metadata: Default::default(),
    }
}

fn ctx_with(prompt: &str) -> AgentContext {
    AgentContext {
        system_prompt: "You are a concise assistant. Answer in a few words.".into(),
        messages: vec![AgentMessage::user(prompt)],
        tools: Arc::from(vec![]),
        model: real_model(),
        thinking_level: None,
        cache_retention: Default::default(),
        session_id: None,
        metadata: Default::default(),
        stream_options: Default::default(),
    }
}

fn stream_fn() -> Option<AnthropicStreamFn> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())?;
    let mut f = AnthropicStreamFn::new(key);
    // The caller points the SDK at a gateway explicitly; the SDK reads no env.
    if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL")
        && !base.is_empty()
    {
        f = f.with_base_url(base);
    }
    Some(f)
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and network"]
async fn live_text_stream_completes_lifecycle() {
    let Some(sf) = stream_fn() else {
        eprintln!("skipping: ANTHROPIC_API_KEY not set");
        return;
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let ctx = ctx_with("Reply with exactly: pong");

    let msg = sf
        .stream(&ctx, CancellationToken::new(), tx)
        .await
        .expect("stream should succeed");

    // A complete assistant message came back.
    let AgentMessage::Assistant {
        content,
        stop_reason,
        usage,
        ..
    } = &msg
    else {
        panic!("expected assistant message");
    };
    assert!(!content.is_empty(), "expected non-empty content");
    assert!(stop_reason.is_some(), "expected a protocol stop_reason");
    assert!(usage.output_tokens > 0, "expected output tokens");

    // The lifecycle events arrived in order: start, >=1 update, end.
    let mut saw_start = false;
    let mut saw_end = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::MessageStart { .. } => saw_start = true,
            AgentEvent::MessageEnd { .. } => saw_end = true,
            _ => {}
        }
    }
    assert!(saw_start, "expected MessageStart");
    assert!(saw_end, "expected MessageEnd");

    eprintln!("stop_reason={stop_reason:?} usage={usage:?}");
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and network"]
async fn abort_mid_stream_returns_aborted() {
    let Some(sf) = stream_fn() else {
        eprintln!("skipping: ANTHROPIC_API_KEY not set");
        return;
    };

    let (tx, _rx) = mpsc::channel::<AgentEvent>(256);
    let ctx = ctx_with("Count from 1 to 1000, one number per line.");
    let signal = CancellationToken::new();
    let signal_clone = signal.clone();

    // Cancel shortly after the request starts.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        signal_clone.cancel();
    });

    let err = sf
        .stream(&ctx, signal, tx)
        .await
        .expect_err("aborted stream should error");

    let is_aborted = err
        .downcast_ref::<ProviderError>()
        .map(|e| matches!(e, ProviderError::Aborted))
        .unwrap_or(false);
    assert!(is_aborted, "expected ProviderError::Aborted, got {err:?}");
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and network"]
async fn live_tool_use_roundtrip() {
    use manox_harness::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
    use manox_harness::types::ContentBlock;
    use serde_json::Value as JsonValue;

    // A trivial tool the model can call.
    struct GetWeather;
    #[async_trait::async_trait]
    impl AgentTool for GetWeather {
        fn name(&self) -> &str {
            "get_weather"
        }
        fn description(&self) -> &str {
            "Get the weather for a city"
        }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            })
        }
        async fn execute(
            &self,
            _id: &str,
            _p: JsonValue,
            _s: CancellationToken,
            _c: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            Ok(AgentToolResult::text("sunny"))
        }
    }

    let Some(sf) = stream_fn() else {
        eprintln!("skipping: ANTHROPIC_API_KEY not set");
        return;
    };

    let mut ctx = ctx_with("What's the weather in Paris? You must call get_weather.");
    ctx.tools = Arc::from(vec![Arc::new(GetWeather) as Arc<dyn AgentTool>]);

    let (tx, _rx) = mpsc::channel::<AgentEvent>(256);
    let msg = sf
        .stream(&ctx, CancellationToken::new(), tx)
        .await
        .expect("tool_use stream should succeed");

    // The model should have emitted a tool_use block with parsed arguments.
    let AgentMessage::Assistant {
        content,
        stop_reason,
        ..
    } = &msg
    else {
        panic!("expected assistant");
    };
    let tool_use = content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
        _ => None,
    });
    let (name, input) = tool_use.expect("expected a tool_use block");
    assert_eq!(name, "get_weather");
    assert!(
        input.get("city").is_some(),
        "expected parsed city arg, got {input}"
    );
    assert_eq!(
        *stop_reason,
        Some(manox_harness::types::StopReason::ToolUse)
    );

    eprintln!("tool_use: {name} input={input}");
}
