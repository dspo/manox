// OpenAI Chat Completions provider.
//
// `wire` mirrors the API schema field-for-field (plus the ecosystem's
// de-facto extensions); `translate` converts between the domain types and
// the wire types; `CompletionsStreamFn` implements `StreamFn` on top of both.

pub mod translate;
pub mod wire;

use futures::StreamExt;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;
use crate::provider::sse::SseParser;
use crate::provider::{ProviderError, overflow, retry};
use crate::types::{
    AgentContext, AgentEvent, AgentMessage, AssistantMessageEvent, ContentBlock, StreamOptions,
    Usage,
};

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
    /// Whether the endpoint reports `finish_reason` (TS
    /// `supportsFinishReason`, default true). A stream that ends without one
    /// is truncated and surfaces an error; only endpoints that explicitly
    /// opt out accept a missing `finish_reason` and infer `stop`/`toolUse`.
    supports_finish_reason: bool,
    request_observer: Option<Arc<dyn crate::provider::RequestObserver>>,
}

impl CompletionsStreamFn {
    pub fn new(api_key: impl Into<String>) -> Self {
        CompletionsStreamFn {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            options: StreamOptions::default(),
            request_observer: None,
            supports_finish_reason: true,
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
        observer: Arc<dyn crate::provider::RequestObserver>,
    ) -> Self {
        self.request_observer = Some(observer);
        self
    }

    /// Override the HTTP client (e.g. to inject a test transport).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Declare that the endpoint never reports `finish_reason`; a stream
    /// without one then infers `stop`/`toolUse` instead of being treated as
    /// truncated (TS `model.compat.supportsFinishReason: false`).
    pub fn with_supports_finish_reason(mut self, supports: bool) -> Self {
        self.supports_finish_reason = supports;
        self
    }
}

#[async_trait::async_trait]
impl StreamFn for CompletionsStreamFn {
    fn api(&self) -> &str {
        "openai_completions"
    }

    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let options = self.options.overlay(&context.stream_options);
        let body = serde_json::to_value(to_request(context, &options, &self.base_url))
            .map_err(|e| anyhow::anyhow!("request payload serialization failed: {e}"))?;
        let url = format!("{}/chat/completions", self.base_url);

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
                    .bearer_auth(&self.api_key)
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
                apply_payload(&mut acc, &payload, &event_tx).await?;
            }
        }

        // Drain any trailing unterminated event.
        if let Some(payload) = parser.finish()
            && payload != "[DONE]"
        {
            apply_payload(&mut acc, &payload, &event_tx).await?;
        }

        // A stream that ended without a `finish_reason` is truncated — even
        // one closed by `[DONE]` — and must not persist a partial message as
        // if it were whole, mirroring the TS throw for a missing
        // finish_reason under the default `supportsFinishReason: true`.
        // Endpoints that never report it opt out via
        // [`CompletionsStreamFn::with_supports_finish_reason`].
        if !acc.has_finish_reason() && self.supports_finish_reason {
            return Err(
                ProviderError::Transport("stream ended without finish_reason".into()).into(),
            );
        }

        acc.finish(&event_tx).await
    }
}

/// Parse one `data:` payload and fold it into the accumulator. A 2xx stream
/// can still carry an error object as data; that is the only payload shape
/// besides chunks.
async fn apply_payload(
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
    acc.apply(chunk, tx).await
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
    /// Response id reported in the first chunk, surfaced as `response_id`.
    response_id: Option<String>,
    /// Model reported in a chunk when the upstream reroutes the request.
    response_model: Option<String>,
    /// Raw `finish_reason` retained so a failure stop reason carries an
    /// `error_message` derived from it.
    raw_finish_reason: Option<String>,
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
            response_id: None,
            response_model: None,
            raw_finish_reason: None,
            blocks: Vec::new(),
            open_text: None,
            open_thinking: None,
            tool_calls: Vec::new(),
            stop_reason: None,
            usage: Box::new(Usage::default()),
            started: false,
        }
    }

    /// Whether a `finish_reason` was reported before the stream ended — the
    /// protocol's terminal marker alongside `[DONE]`.
    fn has_finish_reason(&self) -> bool {
        self.raw_finish_reason.is_some()
    }

    fn current(&self) -> AgentMessage {
        // A failure finish reason (content_filter/network_error) surfaces its
        // raw label as the message's `error_message`.
        let error_message = match self.stop_reason {
            Some(crate::types::StopReason::Error) => Some(format!(
                "provider finish reason: {}",
                self.raw_finish_reason.as_deref().unwrap_or("error")
            )),
            _ => None,
        };
        AgentMessage::Assistant {
            content: self.blocks.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            api: "openai_completions".into(),
            response_model: self.response_model.clone(),
            response_id: self.response_id.clone(),
            diagnostics: None,
            stop_reason: self.stop_reason,
            raw_stop_reason: self.raw_finish_reason.clone(),
            usage: self.usage.clone(),
            error_message,
            timestamp: chrono::Utc::now(),
        }
    }

    async fn apply(
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
        // Capture the response id and (possibly rerouted) model from the
        // first chunk that carries them.
        if let Some(id) = &chunk.id {
            self.response_id = Some(id.clone());
        }
        if let Some(m) = &chunk.model {
            self.response_model = Some(m.clone());
        }
        if !self.started {
            self.started = true;
            let _ = tx
                .send(AgentEvent::MessageStart {
                    message: Box::new(self.current()),
                })
                .await;
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            return Ok(());
        };
        if let Some(reason) = &choice.finish_reason {
            self.raw_finish_reason = Some(reason.clone());
            self.stop_reason = Some(parse_finish_reason(reason));
        }
        let Some(delta) = choice.delta else {
            return Ok(());
        };

        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            self.push_text(&text, &mut events);
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
            self.push_thinking(&thinking, &mut events);
        }
        if let Some(calls) = delta.tool_calls {
            for call in calls {
                self.apply_tool_call(call, &mut events);
            }
        }

        for assistant_message_event in events {
            let _ = tx
                .send(AgentEvent::MessageUpdate {
                    message: Box::new(self.current()),
                    assistant_message_event,
                })
                .await;
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str, events: &mut Vec<AssistantMessageEvent>) {
        // One text block per stream, mirroring TS: interleaved reasoning does
        // not close the open text block — deltas keep appending to it.
        let index = match self.open_text {
            Some(i) => i,
            None => {
                self.blocks.push(ContentBlock::Text {
                    text: String::new(),
                    signature: None,
                });
                let i = self.blocks.len() - 1;
                self.open_text = Some(i);
                events.push(AssistantMessageEvent::TextStart { content_index: i });
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
        events.push(AssistantMessageEvent::TextDelta {
            content_index: index,
            delta: text.to_string(),
        });
    }

    fn push_thinking(&mut self, thinking: &str, events: &mut Vec<AssistantMessageEvent>) {
        // One thinking block per stream, mirroring TS: interleaved text does
        // not close the open thinking block.
        let index = match self.open_thinking {
            Some(i) => i,
            None => {
                // The signature slot stays empty: Completions history never
                // replays reasoning, and a foreign signature would poison a
                // later Anthropic-formatted turn of the same session.
                self.blocks.push(ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                    redacted: None,
                });
                let i = self.blocks.len() - 1;
                self.open_thinking = Some(i);
                events.push(AssistantMessageEvent::ThinkingStart { content_index: i });
                i
            }
        };
        if let ContentBlock::Thinking { thinking: t, .. } = &mut self.blocks[index] {
            t.push_str(thinking);
        }
        events.push(AssistantMessageEvent::ThinkingDelta {
            content_index: index,
            delta: thinking.to_string(),
        });
    }

    fn apply_tool_call(
        &mut self,
        call: WireToolCallDelta,
        events: &mut Vec<AssistantMessageEvent>,
    ) {
        while self.tool_calls.len() <= call.index {
            let block_index = self.blocks.len();
            self.blocks.push(ContentBlock::ToolUse {
                id: String::new(),
                name: String::new(),
                input: JsonValue::Null,
                thought_signature: None,
            });
            self.tool_calls.push(ToolCallAcc {
                block_index,
                id: String::new(),
                name: String::new(),
                args_json: String::new(),
            });
            events.push(AssistantMessageEvent::ToolCallStart {
                content_index: block_index,
            });
        }

        let mut args_delta = String::new();
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
                args_delta = args;
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
        // One delta event per wire delta, even when it carried only identity
        // fields (the fragment is empty then).
        events.push(AssistantMessageEvent::ToolCallDelta {
            content_index: self.tool_calls[call.index].block_index,
            delta: args_delta,
        });
    }

    async fn finish(
        mut self,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        // A stream terminated by `[DONE]` alone carries no finish_reason;
        // infer the completed stop reason the way TS does for endpoints
        // without finish_reason support.
        if self.stop_reason.is_none() {
            self.stop_reason = Some(
                if self
                    .blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
                {
                    crate::types::StopReason::ToolUse
                } else {
                    crate::types::StopReason::Stop
                },
            );
        }
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
        // Content blocks end only at stream end, one *_end per block in order.
        for (index, block) in self.blocks.iter().enumerate() {
            let assistant_message_event = match block {
                ContentBlock::Text { text, .. } => AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text.clone(),
                },
                ContentBlock::Thinking { thinking, .. } => AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.clone(),
                },
                ContentBlock::ToolUse { .. } => AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: block.clone(),
                },
                _ => continue,
            };
            let _ = tx
                .send(AgentEvent::MessageUpdate {
                    message: Box::new(self.current()),
                    assistant_message_event,
                })
                .await;
        }
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

    use wire::{WireChoice, WireDelta, WireFunctionDelta, WirePromptTokensDetails, WireUsage};

    fn ctx() -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages: Vec::new(),
            tools: Arc::from(vec![]),
            model: Model {
                provider: "openai".into(),
                id: "gpt-test".into(),
                api: "openai_completions".into(),
                context_window: 200_000,
                max_tokens: 16_384,
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

    #[tokio::test]
    async fn text_stream_produces_lifecycle_events() {
        let (tx, rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(chunk(text("Hello"), None), &tx).await.unwrap();
        acc.apply(chunk(text(", world"), Some("stop")), &tx)
            .await
            .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

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

    #[tokio::test]
    async fn finish_reason_is_kept_as_raw_stop_reason() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(chunk(text("hi"), Some("stop")), &tx)
            .await
            .unwrap();
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
        assert_eq!(raw_stop_reason.as_deref(), Some("stop"));
    }

    /// A stream terminated by `[DONE]` without `finish_reason` still infers
    /// the completed stop reason.
    #[tokio::test]
    async fn done_only_stream_infers_stop_reason() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(chunk(text("hi"), None), &tx).await.unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant { stop_reason, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Stop));
    }

    #[tokio::test]
    async fn reasoning_then_text_becomes_two_blocks() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        let reasoning = |s: &str| WireDelta {
            reasoning_content: Some(s.into()),
            ..Default::default()
        };
        acc.apply(chunk(reasoning("let me"), None), &tx)
            .await
            .unwrap();
        acc.apply(chunk(reasoning(" think"), None), &tx)
            .await
            .unwrap();
        acc.apply(chunk(text("answer"), Some("stop")), &tx)
            .await
            .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(content.len(), 2);
        assert!(
            matches!(&content[0], ContentBlock::Thinking { thinking, signature, .. }
            if thinking == "let me think" && signature.is_none())
        );
        assert!(matches!(&content[1], ContentBlock::Text { text, .. } if text == "answer"));
    }

    /// Interleaved text and reasoning keep exactly two blocks for the whole
    /// stream — each kind merges into its single block, mirroring TS.
    #[tokio::test]
    async fn interleaved_text_and_thinking_merge_into_two_blocks() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        let reasoning = |s: &str| WireDelta {
            reasoning_content: Some(s.into()),
            ..Default::default()
        };
        acc.apply(chunk(text("hello "), None), &tx).await.unwrap();
        acc.apply(chunk(reasoning("think"), None), &tx)
            .await
            .unwrap();
        acc.apply(chunk(text("world"), None), &tx).await.unwrap();
        acc.apply(chunk(reasoning(" more"), None), &tx)
            .await
            .unwrap();
        acc.apply(chunk(text("!"), Some("stop")), &tx)
            .await
            .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(content.len(), 2, "{content:?}");
        let text_joined = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let thinking_joined = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text_joined, "hello world!");
        assert_eq!(thinking_joined, "think more");
    }

    #[tokio::test]
    async fn reasoning_spelling_variants_all_map_to_thinking() {
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
            acc.apply(chunk(value, Some("stop")), &tx).await.unwrap();
            let msg = acc.finish(&tx).await.unwrap();
            let AgentMessage::Assistant { content, .. } = &msg else {
                panic!("expected assistant")
            };
            assert!(
                matches!(&content[0], ContentBlock::Thinking { thinking, .. } if thinking == "r"),
                "{field} must map to a Thinking block"
            );
        }
    }

    #[tokio::test]
    async fn parallel_tool_calls_assemble_across_deltas() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        acc.apply(
            chunk(tool_delta(0, Some("c1"), Some("read"), Some("{\"pa")), None),
            &tx,
        )
        .await
        .unwrap();
        acc.apply(
            chunk(
                tool_delta(1, Some("c2"), Some("bash"), Some("{\"cmd\":\"ls\"}")),
                None,
            ),
            &tx,
        )
        .await
        .unwrap();
        acc.apply(
            chunk(
                tool_delta(0, None, None, Some("th\":\"x\"}")),
                Some("tool_calls"),
            ),
            &tx,
        )
        .await
        .unwrap();
        let msg = acc.finish(&tx).await.unwrap();

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
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "read");
                assert_eq!(*input, serde_json::json!({"path": "x"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        match &content[1] {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "c2");
                assert_eq!(name, "bash");
                assert_eq!(*input, serde_json::json!({"cmd": "ls"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_call_without_arguments_defaults_to_empty_object() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(
            chunk(
                tool_delta(0, Some("c1"), Some("ping"), None),
                Some("tool_calls"),
            ),
            &tx,
        )
        .await
        .unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert!(
            matches!(&content[0], ContentBlock::ToolUse { input, .. } if *input == serde_json::json!({}))
        );
    }

    #[tokio::test]
    async fn malformed_tool_args_error_at_finish() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(
            chunk(
                tool_delta(0, Some("c1"), Some("read"), Some("{not json")),
                Some("tool_calls"),
            ),
            &tx,
        )
        .await
        .unwrap();
        assert!(acc.finish(&tx).await.is_err());
    }

    #[tokio::test]
    async fn usage_comes_from_final_chunk_or_choice() {
        let wire_usage = || WireUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(10),
            prompt_cache_hit_tokens: None,
            prompt_tokens_details: Some(WirePromptTokensDetails {
                cached_tokens: Some(40),
            }),
            completion_tokens_details: None,
        };

        // On the chunk (the standard position).
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let mut c = chunk(text("hi"), Some("stop"));
        c.usage = Some(wire_usage());
        acc.apply(c, &tx).await.unwrap();
        let msg = acc.finish(&tx).await.unwrap();
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
        acc.apply(c, &tx).await.unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant { usage, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(usage.input_tokens, 60);
    }

    #[tokio::test]
    async fn usage_only_chunk_without_choices_is_accepted() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        acc.apply(chunk(text("hi"), Some("stop")), &tx)
            .await
            .unwrap();
        let c = WireChunk {
            usage: Some(WireUsage {
                prompt_tokens: Some(5),
                completion_tokens: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        acc.apply(c, &tx).await.unwrap();
        let msg = acc.finish(&tx).await.unwrap();
        let AgentMessage::Assistant { usage, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(usage.input_tokens, 5);
    }

    #[tokio::test]
    async fn error_payload_mid_stream_becomes_midstream_error() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let err = apply_payload(
            &mut acc,
            "{\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}",
            &tx,
        )
        .await
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
        .await
        .unwrap();
    }

    fn fixture_stream_fn(addr: &str) -> CompletionsStreamFn {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        CompletionsStreamFn {
            client,
            api_key: "test-key".into(),
            base_url: format!("http://{addr}"),
            options: StreamOptions::default(),
            request_observer: None,
            supports_finish_reason: true,
        }
    }

    /// Serve one SSE body then close the connection.
    async fn serve_one(body: &'static str) -> String {
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

    /// A stream that ends before the terminal marker — no `[DONE]` and no
    /// `finish_reason` — surfaces a transport error instead of completing a
    /// partial message.
    #[tokio::test]
    async fn truncated_stream_surfaces_transport_error() {
        let addr = serve_one(
            "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        )
        .await;
        let stream_fn = fixture_stream_fn(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let err = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::Transport(_))
        ));
    }

    /// A stream closed by `[DONE]` without a `finish_reason` is truncated
    /// under the default strict mode (`supportsFinishReason: true`) — only
    /// an endpoint that explicitly opts out accepts it.
    #[tokio::test]
    async fn done_terminated_stream_without_finish_reason_is_truncated() {
        let body = "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: [DONE]\n\n";
        let addr = serve_one(body).await;
        let stream_fn = fixture_stream_fn(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let err = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::Transport(_))
        ));
    }

    /// An endpoint declared without `finish_reason` support completes a
    /// `[DONE]`-closed stream with the inferred stop reason (TS
    /// `supportsFinishReason: false`).
    #[tokio::test]
    async fn compat_endpoint_without_finish_reason_infers_stop() {
        let body = "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: [DONE]\n\n";
        let addr = serve_one(body).await;
        let stream_fn = fixture_stream_fn(&addr).with_supports_finish_reason(false);
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
        assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "hi"));
    }

    /// A `finish_reason` in the final chunk terminates the stream even when
    /// the server never sends `[DONE]`.
    #[tokio::test]
    async fn finish_reason_terminates_stream_without_done() {
        let body = "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
        let addr = serve_one(body).await;
        let stream_fn = fixture_stream_fn(&addr);
        let (tx, _rx) = mpsc::channel(64);
        let message = stream_fn
            .stream(&ctx(), CancellationToken::new(), tx)
            .await
            .unwrap();
        let AgentMessage::Assistant { stop_reason, .. } = &message else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Stop));
    }
}
