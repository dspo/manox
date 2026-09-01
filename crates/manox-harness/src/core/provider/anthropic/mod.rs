// Anthropic Messages API provider.
//
// `wire` mirrors the API schema field-for-field; `translate` converts between
// the domain types and the wire types; `AnthropicStreamFn` implements
// `StreamFn` on top of both.

pub mod translate;
pub mod wire;

use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;
use crate::core::provider::sse::SseParser;
use crate::core::provider::{ProviderError, retry};
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
    request_observer: Option<Arc<dyn crate::core::provider::RequestObserver>>,
}

impl AnthropicStreamFn {
    pub fn new(api_key: impl Into<String>) -> Self {
        AnthropicStreamFn {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            options: StreamOptions::default(),
            request_observer: None,
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

    /// Attach a request observer that fires around every HTTP attempt.
    pub fn with_request_observer(
        mut self,
        observer: Arc<dyn crate::core::provider::RequestObserver>,
    ) -> Self {
        self.request_observer = Some(observer);
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
        // Per-request options from the harness turn snapshot overlay the
        // builder's own; request-set fields win.
        let options = self.options.overlay(&context.stream_options);
        let body = serde_json::to_value(to_request(context, &options))
            .map_err(|e| anyhow::anyhow!("request payload serialization failed: {e}"))?;
        let url = format!("{}/v1/messages", self.base_url);

        // Extra headers are merged into every request; invalid names or
        // values are a configuration bug and surface at the first call.
        let extra_headers: reqwest::header::HeaderMap = options
            .headers
            .iter()
            .filter_map(|(k, v)| {
                let name = reqwest::header::HeaderName::from_bytes(k.as_bytes()).ok()?;
                let value = v.parse().ok()?;
                Some((name, value))
            })
            .collect();
        let response = retry::send_with_retry(
            |payload| {
                let mut builder = self
                    .client
                    .post(&url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .header("content-type", "application/json")
                    .headers(extra_headers.clone())
                    .json(payload);
                if let Some(timeout) = options.timeout {
                    builder = builder.timeout(timeout);
                }
                builder
            },
            self.request_observer.as_deref(),
            &context.model,
            &body,
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
                acc.apply(event, &event_tx).await?;
            }
        }

        // Drain any trailing unterminated event. A malformed final payload is
        // a parse failure like any other — only `[DONE]` (which the feed loop
        // skips too) is tolerated.
        if let Some(payload) = parser.finish()
            && payload != "[DONE]"
        {
            let event: RawStreamEvent =
                serde_json::from_str(&payload).map_err(ProviderError::Json)?;
            acc.apply(event, &event_tx).await?;
        }

        // A stream that began but never reached `message_stop` was cut short,
        // and a stream that never reported a stop reason is incomplete — both
        // surface as failures rather than persisting a partial reply, mirroring
        // the TS throws on a missing message_stop / a pending stop reason.
        if acc.started() && !acc.message_stop_seen() {
            return Err(ProviderError::MidStream(
                "Anthropic stream ended before message_stop".into(),
            )
            .into());
        }
        if acc.stop_reason().is_none() {
            return Err(ProviderError::MidStream(
                "Anthropic stream ended without a stop reason".into(),
            )
            .into());
        }

        acc.finish(&event_tx).await
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
    /// The refusal explanation from `message_delta.stop_details`, surfaced as
    /// the message's `error_message` so callers keep the provider's specific
    /// rejection reason.
    stop_details_explanation: Option<String>,
    blocks: Vec<ContentBlock>,
    /// Raw partial JSON for the tool_use block currently streaming, by index.
    open_json: std::collections::HashMap<usize, String>,
    stop_reason: Option<crate::types::StopReason>,
    usage: Box<Usage>,
    /// Rate card captured from the turn model at construction; priced
    /// onto every usage snapshot in `current()`.
    cost_rates: Option<crate::types::Cost>,
    started: bool,
    /// Whether the protocol's terminal `message_stop` event arrived. A
    /// stream that began but never reached it was cut short.
    message_stop_seen: bool,
}

impl Accumulator {
    fn new(context: &AgentContext) -> Self {
        Accumulator {
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            response_id: None,
            response_model: None,
            raw_stop_reason: None,
            stop_details_explanation: None,
            blocks: Vec::new(),
            open_json: std::collections::HashMap::new(),
            stop_reason: None,
            usage: Box::new(Usage::default()),
            cost_rates: crate::core::provider::model_cost_rates(&context.model),
            started: false,
            message_stop_seen: false,
        }
    }

    fn started(&self) -> bool {
        self.started
    }

    fn message_stop_seen(&self) -> bool {
        self.message_stop_seen
    }

    fn stop_reason(&self) -> Option<crate::types::StopReason> {
        self.stop_reason
    }

    fn current(&self) -> AgentMessage {
        // A refusal surfaces its provider explanation as the `error_message`;
        // other failure stop reasons (sensitive/overflow) carry the raw
        // protocol label so callers can tell why the turn failed without
        // parsing stop_reason alone.
        let error_message = match self.stop_reason {
            Some(crate::types::StopReason::Error) => {
                if self.raw_stop_reason.as_deref() == Some("refusal") {
                    Some(
                        self.stop_details_explanation
                            .clone()
                            .unwrap_or_else(|| "The model refused to complete the request".into()),
                    )
                } else {
                    Some(format!(
                        "provider stop reason: {}",
                        self.raw_stop_reason.as_deref().unwrap_or("error")
                    ))
                }
            }
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
            raw_stop_reason: self.raw_stop_reason.clone(),
            usage: {
                let mut usage = self.usage.clone();
                if let Some(rates) = &self.cost_rates {
                    usage.cost = Some(crate::core::provider::price_usage(rates, &usage));
                }
                usage
            },
            error_message,
            timestamp: chrono::Utc::now(),
        }
    }

    async fn apply(
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
                let _ = tx
                    .send(AgentEvent::MessageStart {
                        message: Box::new(self.current()),
                    })
                    .await;
            }
            RawStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.ensure_index(index);
                let event = match content_block {
                    WireContentBlock::Text { text } => {
                        // The start event may already carry content; deltas
                        // append onto it.
                        self.blocks[index] = ContentBlock::Text {
                            text,
                            signature: None,
                        };
                        AssistantMessageEvent::TextStart {
                            content_index: index,
                        }
                    }
                    WireContentBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        self.blocks[index] = ContentBlock::Thinking {
                            thinking,
                            signature,
                            redacted: None,
                        };
                        AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                        }
                    }
                    WireContentBlock::RedactedThinking { data } => {
                        self.blocks[index] = ContentBlock::Thinking {
                            thinking: "[Reasoning redacted]".into(),
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
                let _ = tx
                    .send(AgentEvent::MessageUpdate {
                        message: Box::new(self.current()),
                        assistant_message_event: event,
                    })
                    .await;
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
                        // The start block may already carry a signature the
                        // delta appends onto.
                        if let ContentBlock::Thinking { signature: s, .. } = &mut self.blocks[index]
                        {
                            s.get_or_insert_with(String::new).push_str(&signature);
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
                let _ = tx
                    .send(AgentEvent::MessageUpdate {
                        message: Box::new(self.current()),
                        assistant_message_event: event,
                    })
                    .await;
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
                let _ = tx
                    .send(AgentEvent::MessageUpdate {
                        message: Box::new(self.current()),
                        assistant_message_event: event,
                    })
                    .await;
            }
            RawStreamEvent::MessageDelta { delta, usage } => {
                if let Some(sr) = &delta.stop_reason {
                    self.raw_stop_reason = Some(sr.clone());
                    self.stop_reason = Some(parse_stop_reason(sr));
                }
                if let Some(details) = &delta.stop_details
                    && details.kind == "refusal"
                {
                    self.stop_details_explanation = details.explanation.clone();
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
            RawStreamEvent::MessageStop => {
                self.message_stop_seen = true;
            }
            RawStreamEvent::Ping => {}
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

    async fn finish(self, tx: &mpsc::Sender<AgentEvent>) -> Result<AgentMessage, anyhow::Error> {
        let message = self.current();
        let _ = tx
            .send(AgentEvent::MessageEnd {
                message: Box::new(message.clone()),
            })
            .await;
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
                api: "anthropic".into(),
                context_window: 200_000,
                max_tokens: 8_192,
                thinking: ThinkingKind::None,
                metadata: Default::default(),
            },
            thinking_level: None,
            cache_retention: Default::default(),
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
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

    #[tokio::test]
    async fn text_stream_produces_lifecycle_events() {
        let (tx, rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(block_start_text(0), &tx).await.unwrap();
        acc.apply(text_delta(0, "Hello"), &tx).await.unwrap();
        acc.apply(text_delta(0, ", world"), &tx).await.unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .await
            .unwrap();
        acc.apply(message_delta("end_turn"), &tx).await.unwrap();
        let msg = acc.finish(&tx).await.unwrap();

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

    #[tokio::test]
    async fn tool_use_partial_json_accumulates_and_parses() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: WireContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::Value::Null,
                },
            },
            &tx,
        )
        .await
        .unwrap();
        acc.apply(json_delta(0, "{\"path\":"), &tx).await.unwrap();
        acc.apply(json_delta(0, "\"x.rs\"}"), &tx).await.unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .await
            .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

        match &msg {
            AgentMessage::Assistant { content, .. } => match &content[0] {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    assert_eq!(id, "t1");
                    assert_eq!(name, "Read");
                    assert_eq!(*input, serde_json::json!({"path": "x.rs"}));
                }
                other => panic!("expected tool_use, got {other:?}"),
            },
            _ => panic!("expected assistant"),
        }
    }

    #[tokio::test]
    async fn thinking_block_keeps_signature() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();
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
        .await
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
        .await
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
        .await
        .unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .await
            .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

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

    #[tokio::test]
    async fn malformed_tool_json_errors() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: WireContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::Value::Null,
                },
            },
            &tx,
        )
        .await
        .unwrap();
        acc.apply(json_delta(0, "{not json"), &tx).await.unwrap();
        let err = acc
            .apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn message_delta_merges_only_present_usage_classes() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();

        // The delta reports output and cache classes but omits input: the
        // message_start input survives; the total is the merged sum.
        acc.apply(
            RawStreamEvent::MessageDelta {
                delta: wire::MessageDeltaBody {
                    stop_reason: Some("end_turn".into()),
                    stop_sequence: None,
                    stop_details: None,
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
        .await
        .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

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

    /// A content_block_start may already carry text, thinking, and a
    /// signature; the deltas that follow append onto those initial values
    /// rather than replacing them.
    #[tokio::test]
    async fn content_block_start_initial_content_is_preserved() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: WireContentBlock::Text {
                    text: "Initial text".into(),
                },
            },
            &tx,
        )
        .await
        .unwrap();
        acc.apply(text_delta(0, " plus delta"), &tx).await.unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 0 }, &tx)
            .await
            .unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 1,
                content_block: WireContentBlock::Thinking {
                    thinking: "Initial thinking".into(),
                    signature: Some("initial signature".into()),
                },
            },
            &tx,
        )
        .await
        .unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockDelta {
                index: 1,
                delta: WireDelta::Thinking {
                    thinking: " plus delta".into(),
                },
            },
            &tx,
        )
        .await
        .unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockDelta {
                index: 1,
                delta: WireDelta::Signature {
                    signature: " plus delta".into(),
                },
            },
            &tx,
        )
        .await
        .unwrap();
        acc.apply(RawStreamEvent::ContentBlockStop { index: 1 }, &tx)
            .await
            .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

        match &msg {
            AgentMessage::Assistant { content, .. } => {
                match &content[0] {
                    ContentBlock::Text { text, .. } => {
                        assert_eq!(text, "Initial text plus delta");
                    }
                    other => panic!("expected text, got {other:?}"),
                }
                match &content[1] {
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                        ..
                    } => {
                        assert_eq!(thinking, "Initial thinking plus delta");
                        assert_eq!(signature.as_deref(), Some("initial signature plus delta"));
                    }
                    other => panic!("expected thinking, got {other:?}"),
                }
            }
            _ => panic!("expected assistant"),
        }
    }

    /// A refusal `message_delta` carries its explanation into the message's
    /// `error_message`, and the raw stop reason persists on the message.
    #[tokio::test]
    async fn refusal_keeps_stop_details_explanation_and_raw_reason() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(
            RawStreamEvent::MessageDelta {
                delta: wire::MessageDeltaBody {
                    stop_reason: Some("refusal".into()),
                    stop_sequence: None,
                    stop_details: Some(wire::WireStopDetails {
                        kind: "refusal".into(),
                        explanation: Some("I cannot help with that.".into()),
                    }),
                },
                usage: None,
            },
            &tx,
        )
        .await
        .unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant {
            stop_reason,
            raw_stop_reason,
            error_message,
            ..
        } = &msg
        else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Error));
        assert_eq!(raw_stop_reason.as_deref(), Some("refusal"));
        assert_eq!(error_message.as_deref(), Some("I cannot help with that."));
    }

    /// A refusal without a provider explanation falls back to the TS wording.
    #[tokio::test]
    async fn refusal_without_explanation_falls_back_to_default() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(message_delta("refusal"), &tx).await.unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant {
            stop_reason,
            raw_stop_reason,
            error_message,
            ..
        } = &msg
        else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Error));
        assert_eq!(raw_stop_reason.as_deref(), Some("refusal"));
        assert_eq!(
            error_message.as_deref(),
            Some("The model refused to complete the request")
        );
    }

    /// A normal end_turn carries its raw stop reason too — the field is not
    /// reserved for failures.
    #[tokio::test]
    async fn normal_stop_keeps_raw_stop_reason() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(message_delta("end_turn"), &tx).await.unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant {
            stop_reason,
            raw_stop_reason,
            ..
        } = &msg
        else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Stop));
        assert_eq!(raw_stop_reason.as_deref(), Some("end_turn"));
    }

    /// Redacted thinking carries the TS `[Reasoning redacted]` placeholder
    /// text instead of an empty string.
    #[tokio::test]
    async fn redacted_thinking_uses_placeholder_text() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(start_event(), &tx).await.unwrap();
        acc.apply(
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: WireContentBlock::RedactedThinking {
                    data: "encrypted-payload".into(),
                },
            },
            &tx,
        )
        .await
        .unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        match &content[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
                redacted,
            } => {
                assert_eq!(thinking, "[Reasoning redacted]");
                assert_eq!(signature.as_deref(), Some("encrypted-payload"));
                assert_eq!(*redacted, Some(true));
            }
            other => panic!("expected thinking, got {other:?}"),
        }
    }

    // ── fixtures ────────────────────────────────────────────────────────────

    fn anthropic_fixture(addr: &str) -> AnthropicStreamFn {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        AnthropicStreamFn {
            client,
            api_key: "test-key".into(),
            base_url: format!("http://{addr}"),
            options: StreamOptions::default(),
            request_observer: None,
        }
    }

    /// Serve one SSE body then close the connection.
    async fn serve_anthropic(body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((mut socket, _)) = listener.accept().await {
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        addr
    }

    const MESSAGE_START: &str = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-test\",\"role\":\"assistant\",\"content\":[],\"stop_reason\":null,\"usage\":{}}}\n\n";
    const MESSAGE_STOP: &str = "data: {\"type\":\"message_stop\"}\n\n";

    /// A stream that began but never reached `message_stop` was cut short:
    /// the partial reply must not be persisted as a completed response.
    #[tokio::test]
    async fn stream_without_message_stop_is_midstream_error() {
        let addr = serve_anthropic(MESSAGE_START.to_string()).await;
        let stream_fn = anthropic_fixture(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let err = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::MidStream(m)) if m.contains("before message_stop")
        ));
    }

    /// A stream that reaches `message_stop` without ever reporting a stop
    /// reason is incomplete — the finalized message must carry one.
    #[tokio::test]
    async fn stream_without_stop_reason_is_midstream_error() {
        let body = format!("{MESSAGE_START}{MESSAGE_STOP}");
        let addr = serve_anthropic(body).await;
        let stream_fn = anthropic_fixture(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let err = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::MidStream(m)) if m.contains("without a stop reason")
        ));
    }

    /// An empty 200 stream is neither a completed response nor a started one:
    /// the missing stop reason surfaces the failure.
    #[tokio::test]
    async fn empty_stream_is_midstream_error() {
        let addr = serve_anthropic(String::new()).await;
        let stream_fn = anthropic_fixture(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let err = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::MidStream(m)) if m.contains("without a stop reason")
        ));
    }

    /// A complete stream — start, delta with a stop reason, stop — still
    /// finishes as a normal message.
    #[tokio::test]
    async fn complete_stream_finishes_normally() {
        let body = format!(
            "{MESSAGE_START}data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"hi\"}}}}\n\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\" there\"}}}}\n\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{}}}}\n\n{MESSAGE_STOP}"
        );
        let addr = serve_anthropic(body).await;
        let stream_fn = anthropic_fixture(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let message = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap();
        let AgentMessage::Assistant {
            stop_reason,
            content,
            ..
        } = &message
        else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Stop));
        assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "hi there"));
    }

    /// A malformed trailing payload (an unterminated final event) is a parse
    /// failure, not a silently skipped tail — the stream must surface it.
    #[tokio::test]
    async fn malformed_trailing_payload_is_a_parse_error() {
        let body = format!(
            "{MESSAGE_START}data: {{{{malformed\n" // no trailing blank line
        );
        let addr = serve_anthropic(body).await;
        let stream_fn = anthropic_fixture(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let err = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::Json(_))
        ));
    }

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
                stop_details: None,
            },
            usage: None,
        }
    }
}
