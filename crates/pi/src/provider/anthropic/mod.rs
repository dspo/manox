// Anthropic Messages API provider.
//
// `wire` mirrors the API schema field-for-field; `translate` converts between
// the domain types and the wire types; `AnthropicStreamFn` implements
// `StreamFn` on top of both.

pub mod translate;
pub mod wire;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;
use crate::provider::sse::SseParser;
use crate::provider::{ProviderError, retry};
use crate::types::{
    AgentContext, AgentEvent, AgentMessage, AssistantMessageEvent, ContentBlock, StreamOptions,
    Usage,
};

use translate::{parse_stop_reason, to_request, to_usage};
use wire::{RawStreamEvent, WireContentBlock, WireDelta};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A `StreamFn` backed by the Anthropic Messages API.
pub struct AnthropicStreamFn {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    options: StreamOptions,
}

impl AnthropicStreamFn {
    pub fn new(api_key: impl Into<String>) -> Self {
        AnthropicStreamFn {
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
impl StreamFn for AnthropicStreamFn {
    fn api(&self) -> &str {
        "anthropic"
    }

    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let body = to_request(context, &self.options);
        let url = format!("{}/v1/messages", self.base_url);

        let response = retry::send_with_retry(
            || {
                self.client
                    .post(&url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .header("content-type", "application/json")
                    .json(&body)
            },
            &signal,
            &event_tx,
        )
        .await?;

        // Consume the SSE byte stream, folding events into an accumulator.
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
                let event: RawStreamEvent =
                    serde_json::from_str(&payload).map_err(ProviderError::Json)?;
                acc.apply(event, &event_tx)?;
            }
        }

        // Drain any trailing unterminated event.
        if let Some(payload) = parser.finish()
            && let Ok(event) = serde_json::from_str::<RawStreamEvent>(&payload)
        {
            acc.apply(event, &event_tx)?;
        }

        acc.finish(&event_tx)
    }
}

/// Folds a stream of protocol events into a complete assistant message while
/// forwarding lifecycle events to subscribers.
struct Accumulator {
    model: String,
    provider: String,
    /// Response id reported in `message_start`, echoed back to callers as
    /// `response_id` on the finalized assistant message.
    response_id: Option<String>,
    /// Model reported in `message_start` when the upstream routes to a
    /// different one than requested (e.g. an alias). `None` until the first
    /// event arrives.
    response_model: Option<String>,
    /// Raw protocol stop-reason string retained so a failure stop reason can
    /// carry an `error_message` derived from it.
    raw_stop_reason: Option<String>,
    blocks: Vec<ContentBlock>,
    /// Raw partial JSON for the tool_use block currently streaming, by index.
    open_json: std::collections::HashMap<usize, String>,
    stop_reason: Option<crate::types::StopReason>,
    usage: Box<Usage>,
    started: bool,
}

impl Accumulator {
    fn new(context: &AgentContext) -> Self {
        Accumulator {
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            response_id: None,
            response_model: None,
            raw_stop_reason: None,
            blocks: Vec::new(),
            open_json: std::collections::HashMap::new(),
            stop_reason: None,
            usage: Box::new(Usage::default()),
            started: false,
        }
    }

    fn current(&self) -> AgentMessage {
        // A failure stop reason (refusal/sensitive/overflow) surfaces its raw
        // protocol label as the message's `error_message` so callers can tell
        // why the turn failed without parsing stop_reason alone.
        let error_message = match self.stop_reason {
            Some(crate::types::StopReason::Error) => Some(format!(
                "provider stop reason: {}",
                self.raw_stop_reason.as_deref().unwrap_or("error")
            )),
            _ => None,
        };
        AgentMessage::Assistant {
            content: self.blocks.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            api: "anthropic".into(),
            response_model: self.response_model.clone(),
            response_id: self.response_id.clone(),
            diagnostics: None,
            stop_reason: self.stop_reason,
            usage: self.usage.clone(),
            error_message,
            timestamp: chrono::Utc::now(),
        }
    }

    fn apply(
        &mut self,
        event: RawStreamEvent,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<(), anyhow::Error> {
        match event {
            RawStreamEvent::MessageStart { message } => {
                if let Some(u) = &message.usage {
                    *self.usage = to_usage(u);
                }
                // Capture the upstream-assigned id and (possibly rerouted)
                // model so the finalized message carries them as response_id
                // and response_model.
                self.response_id = message.id.clone();
                if let Some(m) = &message.model {
                    self.response_model = Some(m.clone());
                }
                self.started = true;
                let _ = tx.try_send(AgentEvent::MessageStart {
                    message: Box::new(self.current()),
                });
            }
            RawStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.ensure_index(index);
                let event = match content_block {
                    WireContentBlock::Text { .. } => {
                        self.blocks[index] = ContentBlock::Text {
                            text: String::new(),
                            signature: None,
                        };
                        AssistantMessageEvent::TextStart {
                            content_index: index,
                        }
                    }
                    WireContentBlock::Thinking { .. } => {
                        self.blocks[index] = ContentBlock::Thinking {
                            thinking: String::new(),
                            signature: None,
                            redacted: None,
                        };
                        AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                        }
                    }
                    WireContentBlock::RedactedThinking { data } => {
                        self.blocks[index] = ContentBlock::Thinking {
                            thinking: String::new(),
                            signature: Some(data),
                            redacted: Some(true),
                        };
                        AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                        }
                    }
                    WireContentBlock::ToolUse { id, name, .. } => {
                        self.open_json.insert(index, String::new());
                        self.blocks[index] = ContentBlock::ToolUse {
                            id,
                            name,
                            input: serde_json::Value::Null,
                            thought_signature: None,
                        };
                        AssistantMessageEvent::ToolCallStart {
                            content_index: index,
                        }
                    }
                    WireContentBlock::Other => return Ok(()),
                };
                let _ = tx.try_send(AgentEvent::MessageUpdate {
                    message: Box::new(self.current()),
                    assistant_message_event: event,
                });
            }
            RawStreamEvent::ContentBlockDelta { index, delta } => {
                self.ensure_index(index);
                let event = match delta {
                    WireDelta::Text { text } => {
                        if let ContentBlock::Text {
                            text: t,
                            signature: None,
                        } = &mut self.blocks[index]
                        {
                            t.push_str(&text);
                        }
                        AssistantMessageEvent::TextDelta {
                            content_index: index,
                            delta: text,
                        }
                    }
                    WireDelta::Thinking { thinking } => {
                        if let ContentBlock::Thinking { thinking: t, .. } = &mut self.blocks[index]
                        {
                            t.push_str(&thinking);
                        }
                        AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta: thinking,
                        }
                    }
                    WireDelta::Signature { signature } => {
                        // Signatures accumulate silently: no incremental event
                        // exists for them, matching the TS Pi event surface.
                        if let ContentBlock::Thinking { signature: s, .. } = &mut self.blocks[index]
                        {
                            *s = Some(signature);
                        }
                        return Ok(());
                    }
                    WireDelta::InputJson { partial_json } => {
                        self.open_json
                            .entry(index)
                            .or_default()
                            .push_str(&partial_json);
                        AssistantMessageEvent::ToolCallDelta {
                            content_index: index,
                            delta: partial_json,
                        }
                    }
                    WireDelta::Other => return Ok(()),
                };
                let _ = tx.try_send(AgentEvent::MessageUpdate {
                    message: Box::new(self.current()),
                    assistant_message_event: event,
                });
            }
            RawStreamEvent::ContentBlockStop { index } => {
                self.ensure_index(index);
                // Resolve a tool_use block's accumulated partial JSON.
                if let Some(raw) = self.open_json.remove(&index) {
                    let input = if raw.trim().is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::from_str(&raw).map_err(ProviderError::Json)?
                    };
                    if let ContentBlock::ToolUse { input: slot, .. } = &mut self.blocks[index] {
                        *slot = input;
                    }
                }
                let event = match &self.blocks[index] {
                    ContentBlock::Text { text, .. } => AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content: text.clone(),
                    },
                    ContentBlock::Thinking { thinking, .. } => AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: thinking.clone(),
                    },
                    tool_call @ ContentBlock::ToolUse { .. } => {
                        AssistantMessageEvent::ToolCallEnd {
                            content_index: index,
                            tool_call: tool_call.clone(),
                        }
                    }
                    _ => return Ok(()),
                };
                let _ = tx.try_send(AgentEvent::MessageUpdate {
                    message: Box::new(self.current()),
                    assistant_message_event: event,
                });
            }
            RawStreamEvent::MessageDelta { delta, usage } => {
                if let Some(sr) = &delta.stop_reason {
                    self.raw_stop_reason = Some(sr.clone());
                    self.stop_reason = Some(parse_stop_reason(sr));
                }
                if let Some(u) = &usage {
                    // The delta usage carries cumulative counts, but any class
                    // may be absent — merge only what is present so the values
                    // captured at message_start survive.
                    let mut merged = self.usage.clone();
                    if let Some(v) = u.input_tokens {
                        merged.input_tokens = v;
                    }
                    if let Some(v) = u.output_tokens {
                        merged.output_tokens = v;
                    }
                    if let Some(v) = u.cache_read_input_tokens {
                        merged.cache_read_input_tokens = v;
                    }
                    if let Some(v) = u.cache_creation_input_tokens {
                        merged.cache_creation_input_tokens = v;
                    }
                    merged.total_tokens = merged.input_tokens
                        + merged.output_tokens
                        + merged.cache_read_input_tokens
                        + merged.cache_creation_input_tokens;
                    self.usage = merged;
                }
            }
            RawStreamEvent::MessageStop | RawStreamEvent::Ping => {}
        }
        Ok(())
    }

    fn ensure_index(&mut self, index: usize) {
        while self.blocks.len() <= index {
            self.blocks.push(ContentBlock::Text {
                text: String::new(),
                signature: None,
            });
        }
    }

    fn finish(self, tx: &mpsc::Sender<AgentEvent>) -> Result<AgentMessage, anyhow::Error> {
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
    use std::sync::Arc;

    fn ctx() -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![]),
            model: Model {
                provider: "anthropic".into(),
                id: "claude-test".into(),
                context_window: 200_000,
                max_tokens: 8_192,
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

    #[test]
    fn text_stream_produces_lifecycle_events() {
        let (tx, rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(start_event(), &tx).unwrap();
        acc.apply(block_start_text(0), &tx).unwrap();
        acc.apply(text_delta(0, "Hello"), &tx).unwrap();
        acc.apply(text_delta(0, ", world"), &tx).unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .unwrap();
        acc.apply(message_delta("end_turn"), &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();

        // Final text assembled.
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

        // Lifecycle: MessageStart, >=1 MessageUpdate, MessageEnd.
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
    fn tool_use_partial_json_accumulates_and_parses() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(start_event(), &tx).unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: WireContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::Value::Null,
                },
            },
            &tx,
        )
        .unwrap();
        acc.apply(json_delta(0, "{\"path\":"), &tx).unwrap();
        acc.apply(json_delta(0, "\"x.rs\"}"), &tx).unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .unwrap();
        let msg = acc.finish(&tx).unwrap();

        match &msg {
            AgentMessage::Assistant { content, .. } => match &content[0] {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    assert_eq!(id, "t1");
                    assert_eq!(name, "read");
                    assert_eq!(*input, serde_json::json!({"path": "x.rs"}));
                }
                other => panic!("expected tool_use, got {other:?}"),
            },
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn thinking_block_keeps_signature() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: WireContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                },
            },
            &tx,
        )
        .unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockDelta {
                index: 0,
                delta: WireDelta::Thinking {
                    thinking: "hmm".into(),
                },
            },
            &tx,
        )
        .unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockDelta {
                index: 0,
                delta: WireDelta::Signature {
                    signature: "sig!".into(),
                },
            },
            &tx,
        )
        .unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .unwrap();
        let msg = acc.finish(&tx).unwrap();

        match &msg {
            AgentMessage::Assistant { content, .. } => match &content[0] {
                ContentBlock::Thinking {
                    thinking,
                    signature,
                    ..
                } => {
                    assert_eq!(thinking, "hmm");
                    assert_eq!(signature.as_deref(), Some("sig!"));
                }
                other => panic!("expected thinking, got {other:?}"),
            },
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn malformed_tool_json_errors() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: WireContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::Value::Null,
                },
            },
            &tx,
        )
        .unwrap();
        acc.apply(json_delta(0, "{not json"), &tx).unwrap();
        let err = acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx);
        assert!(err.is_err());
    }

    #[test]
    fn message_delta_merges_only_present_usage_classes() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).unwrap();

        // The delta reports output and cache classes but omits input: the
        // message_start input survives; the total is the merged sum.
        acc.apply(
            RawStreamEvent::MessageDelta {
                delta: wire::MessageDeltaBody {
                    stop_reason: Some("end_turn".into()),
                    stop_sequence: None,
                },
                usage: Some(wire::WireUsage {
                    input_tokens: None,
                    output_tokens: Some(7),
                    cache_read_input_tokens: Some(100),
                    cache_creation_input_tokens: Some(20),
                    cache_creation: None,
                }),
            },
            &tx,
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();

        match &msg {
            AgentMessage::Assistant { usage, .. } => {
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.cache_read_input_tokens, 100);
                assert_eq!(usage.cache_creation_input_tokens, 20);
                assert_eq!(usage.total_tokens, 137);
            }
            _ => panic!("expected assistant"),
        }
    }

    // ── fixtures ────────────────────────────────────────────────────────────

    fn start_event() -> RawStreamEvent {
        RawStreamEvent::MessageStart {
            message: wire::WireMessage {
                id: Some("m1".into()),
                model: Some("claude-test".into()),
                role: Some("assistant".into()),
                content: Vec::new(),
                stop_reason: None,
                usage: Some(wire::WireUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(0),
                    cache_read_input_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    cache_creation: None,
                }),
            },
        }
    }

    fn block_start_text(index: usize) -> RawStreamEvent {
        RawStreamEvent::ContentBlockStart {
            index,
            content_block: WireContentBlock::Text {
                text: String::new(),
            },
        }
    }

    fn text_delta(index: usize, text: &str) -> RawStreamEvent {
        RawStreamEvent::ContentBlockDelta {
            index,
            delta: WireDelta::Text { text: text.into() },
        }
    }

    fn json_delta(index: usize, partial: &str) -> RawStreamEvent {
        RawStreamEvent::ContentBlockDelta {
            index,
            delta: WireDelta::InputJson {
                partial_json: partial.into(),
            },
        }
    }

    fn message_delta(stop: &str) -> RawStreamEvent {
        RawStreamEvent::MessageDelta {
            delta: wire::MessageDeltaBody {
                stop_reason: Some(stop.into()),
                stop_sequence: None,
            },
            usage: None,
        }
    }
}
