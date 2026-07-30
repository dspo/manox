// OpenAI Chat Completions provider.
//
// `wire` mirrors the API schema field-for-field (plus the ecosystem's
// de-facto extensions); `translate` converts between the domain types and
// the wire types; `CompletionsStreamFn` implements `StreamFn` on top of both.

pub mod translate;
pub mod wire;

use futures::StreamExt;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;
use crate::provider::sse::SseParser;
use crate::provider::{ProviderError, overflow, retry};
use crate::types::{AgentContext, AgentEvent, AgentMessage, ContentBlock, StreamOptions, Usage};

use translate::{parse_finish_reason, to_request, to_usage};
use wire::{WireChunk, WireErrorPayload, WireToolCallDelta};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// A `StreamFn` backed by the OpenAI Chat Completions API and compatible
/// endpoints.
pub struct CompletionsStreamFn {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    options: StreamOptions,
}

impl CompletionsStreamFn {
    pub fn new(api_key: impl Into<String>) -> Self {
        CompletionsStreamFn {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            options: StreamOptions::default(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_options(mut self, options: StreamOptions) -> Self {
        self.options = options;
        self
    }

    /// Override the HTTP client (e.g. to inject a test transport).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }
}

#[async_trait::async_trait]
impl StreamFn for CompletionsStreamFn {
    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let body = to_request(context, &self.options, &self.base_url);
        let url = format!("{}/chat/completions", self.base_url);

        let response = retry::send_with_retry(
            || {
                self.client
                    .post(&url)
                    .bearer_auth(&self.api_key)
                    .header("content-type", "application/json")
                    .json(&body)
            },
            &signal,
            &event_tx,
        )
        .await?;

        // Consume the SSE byte stream, folding chunks into an accumulator.
        let mut acc = Accumulator::new(context);
        let mut parser = SseParser::new();
        let mut byte_stream = response.bytes_stream();

        loop {
            let chunk = tokio::select! {
                _ = signal.cancelled() => return Err(ProviderError::Aborted.into()),
                c = byte_stream.next() => c,
            };

            let Some(chunk) = chunk else { break }; // stream ended
            let bytes = chunk.map_err(|e| ProviderError::Transport(e.to_string()))?;

            for payload in parser.feed(&bytes) {
                if payload == "[DONE]" {
                    continue;
                }
                apply_payload(&mut acc, &payload, &event_tx)?;
            }
        }

        // Drain any trailing unterminated event.
        if let Some(payload) = parser.finish()
            && payload != "[DONE]"
        {
            apply_payload(&mut acc, &payload, &event_tx)?;
        }

        acc.finish(&event_tx)
    }
}

/// Parse one `data:` payload and fold it into the accumulator. A 2xx stream
/// can still carry an error object as data; that is the only payload shape
/// besides chunks.
fn apply_payload(
    acc: &mut Accumulator,
    payload: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(), anyhow::Error> {
    if let Ok(err) = serde_json::from_str::<WireErrorPayload>(payload) {
        let detail = err
            .error
            .get("message")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.error.to_string());
        return Err(overflow::mid_stream(detail).into());
    }
    let chunk: WireChunk = serde_json::from_str(payload).map_err(ProviderError::Json)?;
    acc.apply(chunk, tx)
}

/// Folds a stream of completion chunks into a complete assistant message
/// while forwarding lifecycle events to subscribers.
///
/// The protocol has no block boundaries: text, reasoning, and tool calls all
/// arrive through one delta channel. Block boundaries are inferred — a run
/// of same-kind deltas grows one block, a kind change opens a new one. Tool
/// calls are indexed by the protocol itself.
struct Accumulator {
    model: String,
    provider: String,
    blocks: Vec<ContentBlock>,
    /// Block currently receiving text deltas; reset when another kind
    /// interrupts the run.
    open_text: Option<usize>,
    /// Block currently receiving reasoning deltas.
    open_thinking: Option<usize>,
    /// Tool calls by protocol index. Argument JSON accumulates raw and is
    /// parsed once the stream completes.
    tool_calls: Vec<ToolCallAcc>,
    stop_reason: Option<crate::types::StopReason>,
    usage: Box<Usage>,
    started: bool,
}

struct ToolCallAcc {
    /// Position of this call's `ToolUse` block in `blocks`.
    block_index: usize,
    id: String,
    name: String,
    args_json: String,
}

impl Accumulator {
    fn new(context: &AgentContext) -> Self {
        Accumulator {
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            blocks: Vec::new(),
            open_text: None,
            open_thinking: None,
            tool_calls: Vec::new(),
            stop_reason: None,
            usage: Box::new(Usage::default()),
            started: false,
        }
    }

    fn current(&self) -> AgentMessage {
        AgentMessage::Assistant {
            content: self.blocks.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            api: "openai_completions".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: self.stop_reason,
            usage: self.usage.clone(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn apply(
        &mut self,
        chunk: WireChunk,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<(), anyhow::Error> {
        if let Some(usage) = chunk
            .usage
            .as_ref()
            .or_else(|| chunk.choices.first().and_then(|c| c.usage.as_ref()))
        {
            *self.usage = to_usage(usage);
        }
        if !self.started {
            self.started = true;
            let _ = tx.try_send(AgentEvent::MessageStart {
                message: Box::new(self.current()),
            });
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            return Ok(());
        };
        if let Some(reason) = &choice.finish_reason {
            self.stop_reason = Some(parse_finish_reason(reason));
        }
        let Some(delta) = choice.delta else {
            return Ok(());
        };

        let mut mutated = false;
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            self.push_text(&text);
            mutated = true;
        }
        // Reasoning arrives under one of several spellings; at most one
        // carries content per chunk, so the first non-empty wins.
        let reasoning = [
            delta.reasoning_content,
            delta.reasoning,
            delta.reasoning_text,
        ]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty());
        if let Some(thinking) = reasoning {
            self.push_thinking(&thinking);
            mutated = true;
        }
        if let Some(calls) = delta.tool_calls {
            for call in calls {
                self.apply_tool_call(call);
            }
            mutated = true;
        }

        if mutated {
            let _ = tx.try_send(AgentEvent::MessageUpdate {
                message: Box::new(self.current()),
            });
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str) {
        // A text delta closes any open reasoning block: interleaved kinds
        // become consecutive blocks.
        self.open_thinking = None;
        let index = match self.open_text {
            Some(i) => i,
            None => {
                self.blocks.push(ContentBlock::Text {
                    text: String::new(),
                    signature: None,
                });
                let i = self.blocks.len() - 1;
                self.open_text = Some(i);
                i
            }
        };
        if let ContentBlock::Text {
            text: t,
            signature: None,
        } = &mut self.blocks[index]
        {
            t.push_str(text);
        }
    }

    fn push_thinking(&mut self, thinking: &str) {
        self.open_text = None;
        let index = match self.open_thinking {
            Some(i) => i,
            None => {
                // The signature slot stays empty: Completions history never
                // replays reasoning, and a foreign signature would poison a
                // later Anthropic-formatted turn of the same session.
                self.blocks.push(ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                });
                let i = self.blocks.len() - 1;
                self.open_thinking = Some(i);
                i
            }
        };
        if let ContentBlock::Thinking { thinking: t, .. } = &mut self.blocks[index] {
            t.push_str(thinking);
        }
    }

    fn apply_tool_call(&mut self, call: WireToolCallDelta) {
        while self.tool_calls.len() <= call.index {
            let block_index = self.blocks.len();
            self.blocks.push(ContentBlock::ToolUse {
                id: String::new(),
                name: String::new(),
                input: JsonValue::Null,
            });
            self.tool_calls.push(ToolCallAcc {
                block_index,
                id: String::new(),
                name: String::new(),
                args_json: String::new(),
            });
        }

        let acc = &mut self.tool_calls[call.index];
        let mut identity_changed = false;
        if let Some(id) = call.id {
            acc.id = id;
            identity_changed = true;
        }
        if let Some(function) = call.function {
            if let Some(name) = function.name {
                acc.name = name;
                identity_changed = true;
            }
            if let Some(args) = function.arguments {
                acc.args_json.push_str(&args);
            }
        }
        if identity_changed {
            let (block_index, id, name) = {
                let a = &self.tool_calls[call.index];
                (a.block_index, a.id.clone(), a.name.clone())
            };
            if let ContentBlock::ToolUse {
                id: bid,
                name: bname,
                ..
            } = &mut self.blocks[block_index]
            {
                *bid = id;
                *bname = name;
            }
        }
    }

    fn finish(mut self, tx: &mpsc::Sender<AgentEvent>) -> Result<AgentMessage, anyhow::Error> {
        // Resolve every tool call's accumulated argument JSON now that the
        // stream is complete.
        for call in &self.tool_calls {
            let input = if call.args_json.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&call.args_json).map_err(ProviderError::Json)?
            };
            if let ContentBlock::ToolUse { input: slot, .. } = &mut self.blocks[call.block_index] {
                *slot = input;
            }
        }
        let message = self.current();
        let _ = tx.try_send(AgentEvent::MessageEnd {
            message: Box::new(message.clone()),
        });
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Model, StopReason, ThinkingKind};
    use wire::{WireChoice, WireDelta, WireFunctionDelta, WirePromptTokensDetails, WireUsage};

    fn ctx() -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: Model {
                provider: "openai".into(),
                id: "gpt-test".into(),
                context_window: 200_000,
                max_tokens: 16_384,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
        }
    }

    fn chan() -> (mpsc::Sender<AgentEvent>, mpsc::Receiver<AgentEvent>) {
        mpsc::channel(64)
    }

    fn drain(mut rx: mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn chunk(delta: WireDelta, finish: Option<&str>) -> WireChunk {
        WireChunk {
            choices: vec![WireChoice {
                delta: Some(delta),
                finish_reason: finish.map(|s| s.into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn text(s: &str) -> WireDelta {
        WireDelta {
            content: Some(s.into()),
            ..Default::default()
        }
    }

    fn tool_delta(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> WireDelta {
        WireDelta {
            tool_calls: Some(vec![WireToolCallDelta {
                index,
                id: id.map(|s| s.into()),
                function: Some(WireFunctionDelta {
                    name: name.map(|s| s.into()),
                    arguments: args.map(|s| s.into()),
                }),
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn text_stream_produces_lifecycle_events() {
        let (tx, rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(chunk(text("Hello"), None), &tx).unwrap();
        acc.apply(chunk(text(", world"), Some("stop")), &tx)
            .unwrap();
        let msg = acc.finish(&tx).unwrap();

        match &msg {
            AgentMessage::Assistant {
                content,
                stop_reason,
                ..
            } => {
                assert!(
                    matches!(&content[0], ContentBlock::Text { text, .. } if text == "Hello, world")
                );
                assert_eq!(*stop_reason, Some(StopReason::Stop));
            }
            _ => panic!("expected assistant"),
        }

        let events = drain(rx);
        assert!(matches!(
            events.first(),
            Some(AgentEvent::MessageStart { .. })
        ));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::MessageUpdate { .. }))
        );
        assert!(matches!(events.last(), Some(AgentEvent::MessageEnd { .. })));
    }

    #[test]
    fn reasoning_then_text_becomes_two_blocks() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        let reasoning = |s: &str| WireDelta {
            reasoning_content: Some(s.into()),
            ..Default::default()
        };
        acc.apply(chunk(reasoning("let me"), None), &tx).unwrap();
        acc.apply(chunk(reasoning(" think"), None), &tx).unwrap();
        acc.apply(chunk(text("answer"), Some("stop")), &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(content.len(), 2);
        assert!(
            matches!(&content[0], ContentBlock::Thinking { thinking, signature }
            if thinking == "let me think" && signature.is_none())
        );
        assert!(matches!(&content[1], ContentBlock::Text { text, .. } if text == "answer"));
    }

    #[test]
    fn reasoning_spelling_variants_all_map_to_thinking() {
        for (field, value) in [
            (
                "reasoning_content",
                WireDelta {
                    reasoning_content: Some("r".into()),
                    ..Default::default()
                },
            ),
            (
                "reasoning",
                WireDelta {
                    reasoning: Some("r".into()),
                    ..Default::default()
                },
            ),
            (
                "reasoning_text",
                WireDelta {
                    reasoning_text: Some("r".into()),
                    ..Default::default()
                },
            ),
        ] {
            let (tx, _rx) = chan();
            let mut acc = Accumulator::new(&ctx());
            acc.apply(chunk(value, Some("stop")), &tx).unwrap();
            let msg = acc.finish(&tx).unwrap();
            let AgentMessage::Assistant { content, .. } = &msg else {
                panic!("expected assistant")
            };
            assert!(
                matches!(&content[0], ContentBlock::Thinking { thinking, .. } if thinking == "r"),
                "{field} must map to a Thinking block"
            );
        }
    }

    #[test]
    fn parallel_tool_calls_assemble_across_deltas() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(
            chunk(tool_delta(0, Some("c1"), Some("read"), Some("{\"pa")), None),
            &tx,
        )
        .unwrap();
        acc.apply(
            chunk(
                tool_delta(1, Some("c2"), Some("bash"), Some("{\"cmd\":\"ls\"}")),
                None,
            ),
            &tx,
        )
        .unwrap();
        acc.apply(
            chunk(
                tool_delta(0, None, None, Some("th\":\"x\"}")),
                Some("tool_calls"),
            ),
            &tx,
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant {
            content,
            stop_reason,
            ..
        } = &msg
        else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::ToolUse));
        assert_eq!(content.len(), 2);
        match &content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "read");
                assert_eq!(*input, serde_json::json!({"path": "x"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        match &content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "c2");
                assert_eq!(name, "bash");
                assert_eq!(*input, serde_json::json!({"cmd": "ls"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_without_arguments_defaults_to_empty_object() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(
            chunk(
                tool_delta(0, Some("c1"), Some("ping"), None),
                Some("tool_calls"),
            ),
            &tx,
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();
        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert!(
            matches!(&content[0], ContentBlock::ToolUse { input, .. } if *input == serde_json::json!({}))
        );
    }

    #[test]
    fn malformed_tool_args_error_at_finish() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(
            chunk(
                tool_delta(0, Some("c1"), Some("read"), Some("{not json")),
                Some("tool_calls"),
            ),
            &tx,
        )
        .unwrap();
        assert!(acc.finish(&tx).is_err());
    }

    #[test]
    fn usage_comes_from_final_chunk_or_choice() {
        let wire_usage = || WireUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(10),
            prompt_cache_hit_tokens: None,
            prompt_tokens_details: Some(WirePromptTokensDetails {
                cached_tokens: Some(40),
            }),
        };

        // On the chunk (the standard position).
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let mut c = chunk(text("hi"), Some("stop"));
        c.usage = Some(wire_usage());
        acc.apply(c, &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();
        let AgentMessage::Assistant { usage, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(usage.input_tokens, 60);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.cache_read_input_tokens, 40);

        // On the choice (the per-choice fallback).
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let c = WireChunk {
            choices: vec![WireChoice {
                delta: Some(text("hi")),
                finish_reason: Some("stop".into()),
                usage: Some(wire_usage()),
            }],
            ..Default::default()
        };
        acc.apply(c, &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();
        let AgentMessage::Assistant { usage, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(usage.input_tokens, 60);
    }

    #[test]
    fn usage_only_chunk_without_choices_is_accepted() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(chunk(text("hi"), Some("stop")), &tx).unwrap();
        let c = WireChunk {
            usage: Some(WireUsage {
                prompt_tokens: Some(5),
                completion_tokens: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        acc.apply(c, &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();
        let AgentMessage::Assistant { usage, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(usage.input_tokens, 5);
    }

    #[test]
    fn error_payload_mid_stream_becomes_midstream_error() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let err = apply_payload(
            &mut acc,
            "{\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}",
            &tx,
        )
        .unwrap_err();
        let e = err.downcast_ref::<ProviderError>().expect("ProviderError");
        assert!(matches!(e, ProviderError::MidStream(m) if m == "boom"));

        // A normal chunk does not trip the error probe.
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        apply_payload(
            &mut acc,
            "{\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}",
            &tx,
        )
        .unwrap();
    }
}
