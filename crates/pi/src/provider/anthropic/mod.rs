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
use crate::provider::ProviderError;
use crate::provider::sse::SseParser;
use crate::types::{AgentContext, AgentEvent, AgentMessage, ContentBlock, StreamOptions, Usage};

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

    /// Build from environment variables: `ANTHROPIC_API_KEY` (required) and
    /// `ANTHROPIC_BASE_URL` (optional, for Anthropic-compatible gateways).
    /// Returns `None` when the key is absent or empty.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty())?;
        let mut f = Self::new(key);
        if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL") {
            if !base.is_empty() {
                f.base_url = base;
            }
        }
        Some(f)
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
    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let body = to_request(context, &self.options);
        let url = format!("{}/v1/messages", self.base_url);

        let request = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body);

        let response = tokio::select! {
            _ = signal.cancelled() => return Err(ProviderError::Aborted.into()),
            res = request.send() => res.map_err(|e| ProviderError::Transport(e.to_string()))?,
        };

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body: body_text,
            }
            .into());
        }

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
        if let Some(payload) = parser.finish() {
            if let Ok(event) = serde_json::from_str::<RawStreamEvent>(&payload) {
                acc.apply(event, &event_tx)?;
            }
        }

        acc.finish(&event_tx)
    }
}

/// Folds a stream of protocol events into a complete assistant message while
/// forwarding lifecycle events to subscribers.
struct Accumulator {
    model: String,
    provider: String,
    blocks: Vec<ContentBlock>,
    /// Raw partial JSON for the tool_use block currently streaming, by index.
    open_json: std::collections::HashMap<usize, String>,
    stop_reason: Option<crate::types::StopReason>,
    usage: Usage,
    started: bool,
}

impl Accumulator {
    fn new(context: &AgentContext) -> Self {
        Accumulator {
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            blocks: Vec::new(),
            open_json: std::collections::HashMap::new(),
            stop_reason: None,
            usage: Usage::default(),
            started: false,
        }
    }

    fn current(&self) -> AgentMessage {
        AgentMessage::Assistant {
            content: self.blocks.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            stop_reason: self.stop_reason,
            usage: self.usage.clone(),
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
                    self.usage = to_usage(u);
                }
                self.started = true;
                let _ = tx.try_send(AgentEvent::MessageStart {
                    message: Box::new(self.current()),
                });
            }
            RawStreamEvent::ContentBlockStart { index, content_block } => {
                self.ensure_index(index);
                match content_block {
                    WireContentBlock::Text { .. } => {
                        self.blocks[index] = ContentBlock::Text { text: String::new() };
                    }
                    WireContentBlock::Thinking { .. } => {
                        self.blocks[index] = ContentBlock::Thinking {
                            thinking: String::new(),
                            signature: None,
                        };
                    }
                    WireContentBlock::RedactedThinking { data } => {
                        self.blocks[index] = ContentBlock::RedactedThinking { data };
                    }
                    WireContentBlock::ToolUse { id, name, .. } => {
                        self.open_json.insert(index, String::new());
                        self.blocks[index] = ContentBlock::ToolUse {
                            id,
                            name,
                            input: serde_json::Value::Null,
                        };
                    }
                    WireContentBlock::Other => {}
                }
            }
            RawStreamEvent::ContentBlockDelta { index, delta } => {
                self.ensure_index(index);
                match delta {
                    WireDelta::Text { text } => {
                        if let ContentBlock::Text { text: t } = &mut self.blocks[index] {
                            t.push_str(&text);
                        }
                    }
                    WireDelta::Thinking { thinking } => {
                        if let ContentBlock::Thinking { thinking: t, .. } = &mut self.blocks[index] {
                            t.push_str(&thinking);
                        }
                    }
                    WireDelta::Signature { signature } => {
                        if let ContentBlock::Thinking { signature: s, .. } = &mut self.blocks[index] {
                            *s = Some(signature);
                        }
                    }
                    WireDelta::InputJson { partial_json } => {
                        self.open_json.entry(index).or_default().push_str(&partial_json);
                    }
                    WireDelta::Other => {}
                }
                let _ = tx.try_send(AgentEvent::MessageUpdate {
                    message: Box::new(self.current()),
                });
            }
            RawStreamEvent::ContentBlockStop { index } => {
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
            }
            RawStreamEvent::MessageDelta { delta, usage } => {
                if let Some(sr) = &delta.stop_reason {
                    self.stop_reason = parse_stop_reason(sr);
                }
                if let Some(u) = &usage {
                    // The delta usage carries cumulative output tokens; merge.
                    let mut merged = self.usage.clone();
                    let delta_usage = to_usage(u);
                    merged.output_tokens = delta_usage.output_tokens;
                    if delta_usage.input_tokens > 0 {
                        merged.input_tokens = delta_usage.input_tokens;
                    }
                    self.usage = merged;
                }
            }
            RawStreamEvent::MessageStop | RawStreamEvent::Ping => {}
        }
        Ok(())
    }

    fn ensure_index(&mut self, index: usize) {
        while self.blocks.len() <= index {
            self.blocks.push(ContentBlock::Text { text: String::new() });
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
    use crate::types::{Model, StopReason};

    fn ctx() -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages: Vec::new(),
            tools: Vec::new(),
            model: Model {
                provider: "anthropic".into(),
                id: "claude-test".into(),
                context_window: 200_000,
                supports_thinking: false,
                metadata: Default::default(),
            },
            thinking_level: None,
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
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx).unwrap();
        acc.apply(message_delta("end_turn"), &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();

        // Final text assembled.
        match &msg {
            AgentMessage::Assistant { content, stop_reason, .. } => {
                assert!(matches!(&content[0], ContentBlock::Text { text } if text == "Hello, world"));
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
            }
            _ => panic!("expected assistant"),
        }

        // Lifecycle: MessageStart, >=1 MessageUpdate, MessageEnd.
        let events = drain(rx);
        assert!(matches!(events.first(), Some(AgentEvent::MessageStart { .. })));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::MessageUpdate { .. })));
        assert!(matches!(events.last(), Some(AgentEvent::MessageEnd { .. })));
    }

    #[test]
    fn tool_use_partial_json_accumulates_and_parses() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(start_event(), &tx).unwrap();
        acc.apply(RawStreamEvent::ContentBlockStart {
            index: 0,
            content_block: WireContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::Value::Null,
            },
        }, &tx).unwrap();
        acc.apply(json_delta(0, "{\"path\":" ), &tx).unwrap();
        acc.apply(json_delta(0, "\"x.rs\"}"), &tx).unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();

        match &msg {
            AgentMessage::Assistant { content, .. } => match &content[0] {
                ContentBlock::ToolUse { id, name, input } => {
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
        acc.apply(RawStreamEvent::ContentBlockStart {
            index: 0,
            content_block: WireContentBlock::Thinking { thinking: String::new(), signature: None },
        }, &tx).unwrap();
        acc.apply(RawStreamEvent::ContentBlockDelta {
            index: 0,
            delta: WireDelta::Thinking { thinking: "hmm".into() },
        }, &tx).unwrap();
        acc.apply(RawStreamEvent::ContentBlockDelta {
            index: 0,
            delta: WireDelta::Signature { signature: "sig!".into() },
        }, &tx).unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx).unwrap();
        let msg = acc.finish(&tx).unwrap();

        match &msg {
            AgentMessage::Assistant { content, .. } => match &content[0] {
                ContentBlock::Thinking { thinking, signature } => {
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
        acc.apply(RawStreamEvent::ContentBlockStart {
            index: 0,
            content_block: WireContentBlock::ToolUse {
                id: "t1".into(), name: "read".into(), input: serde_json::Value::Null,
            },
        }, &tx).unwrap();
        acc.apply(json_delta(0, "{not json"), &tx).unwrap();
        let err = acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx);
        assert!(err.is_err());
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
                    input_tokens: 10,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_creation: None,
                }),
            },
        }
    }

    fn block_start_text(index: usize) -> RawStreamEvent {
        RawStreamEvent::ContentBlockStart {
            index,
            content_block: WireContentBlock::Text { text: String::new() },
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
            delta: WireDelta::InputJson { partial_json: partial.into() },
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
