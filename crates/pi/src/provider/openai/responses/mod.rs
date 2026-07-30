// OpenAI Responses provider.
//
// `wire` mirrors the API schema field-for-field; `translate` converts
// between the domain types and the wire types; `ResponsesStreamFn`
// implements `StreamFn` on top of both.
//
// The stream is a flat event sequence keyed by `output_index`. Each output
// item (reasoning / message / function_call) occupies one slot from
// `output_item.added` to `output_item.done`; deltas address their slot by
// index. Reasoning items are captured raw on completion — the serialized
// item becomes the Thinking block's signature, which is what makes
// client-side replay possible under `store: false`.

pub mod translate;
pub mod wire;

use std::collections::HashMap;

use futures::StreamExt;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::StreamFn;
use crate::provider::sse::SseParser;
use crate::provider::{ProviderError, overflow, retry};
use crate::types::{AgentContext, AgentEvent, AgentMessage, ContentBlock, StreamOptions, Usage};

use translate::{encode_text_signature, to_request, to_usage};
use wire::{
    WireArgumentsDoneEvent, WireDeltaEvent, WireErrorEvent, WireErrorPayload, WireFunctionCall,
    WireIndexEvent, WireOutputItemEvent, WireOutputMessage, WireResponse, WireResponseEvent,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// A `StreamFn` backed by the OpenAI Responses API and compatible endpoints.
pub struct ResponsesStreamFn {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    options: StreamOptions,
}

impl ResponsesStreamFn {
    pub fn new(api_key: impl Into<String>) -> Self {
        ResponsesStreamFn {
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
impl StreamFn for ResponsesStreamFn {
    async fn stream(
        &self,
        context: &AgentContext,
        signal: CancellationToken,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentMessage, anyhow::Error> {
        let body = to_request(context, &self.options);
        let url = format!("{}/responses", self.base_url);

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

/// Parse one `data:` payload and fold it into the accumulator. Events are
/// discriminated by their `type` tag; a payload without one is an error
/// envelope — some endpoints stream their HTTP error body (`{"error": ...}`
/// or a bare `{"code", "message"}` pair) as data on a 2xx response — or not
/// ours to interpret. Unknown event kinds are skipped: the API adds event
/// types over time, and ignoring them keeps the stream forward-compatible.
fn apply_payload(
    acc: &mut Accumulator,
    payload: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(), anyhow::Error> {
    let value: JsonValue = serde_json::from_str(payload).map_err(ProviderError::Json)?;
    let kind = value
        .get("type")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let Some(kind) = kind else {
        if let Ok(err) = serde_json::from_value::<WireErrorPayload>(value.clone()) {
            let detail = err
                .error
                .get("message")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| err.error.to_string());
            return Err(overflow::mid_stream(detail).into());
        }
        if value.get("message").is_some() || value.get("code").is_some() {
            let ev: WireErrorEvent = serde_json::from_value(value).map_err(ProviderError::Json)?;
            let detail = ev.message.unwrap_or_else(|| {
                ev.code
                    .map(|c| format!("error code {c}"))
                    .unwrap_or_else(|| "unknown error".to_string())
            });
            return Err(overflow::mid_stream(detail).into());
        }
        return Ok(());
    };
    acc.apply(&kind, value, tx)
}

/// The output slot an `output_index` currently holds.
enum Slot {
    Thinking {
        block: usize,
    },
    Text {
        block: usize,
    },
    /// Argument JSON accumulates raw and is parsed once the item completes.
    ToolCall {
        block: usize,
        args_json: String,
    },
}

/// Folds a stream of response events into a complete assistant message
/// while forwarding lifecycle events to subscribers.
struct Accumulator {
    model: String,
    provider: String,
    blocks: Vec<ContentBlock>,
    /// Open output slots by their protocol `output_index`.
    slots: HashMap<usize, Slot>,
    /// Reasoning item id -> block index, for the terminal-signature
    /// backfill (some endpoints only attach `encrypted_content` on the
    /// completed response, not on `output_item.done`).
    reasoning_blocks: HashMap<String, usize>,
    stop_reason: Option<crate::types::StopReason>,
    usage: Usage,
    started: bool,
    /// A `response.completed` / `response.incomplete` / `response.failed`
    /// event arrived. A stream that ends without one is a protocol
    /// violation.
    terminal_seen: bool,
}

impl Accumulator {
    fn new(context: &AgentContext) -> Self {
        Accumulator {
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            blocks: Vec::new(),
            slots: HashMap::new(),
            reasoning_blocks: HashMap::new(),
            stop_reason: None,
            usage: Usage::default(),
            started: false,
            terminal_seen: false,
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
        kind: &str,
        value: JsonValue,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<(), anyhow::Error> {
        if !self.started {
            self.started = true;
            let _ = tx.try_send(AgentEvent::MessageStart {
                message: Box::new(self.current()),
            });
        }

        let mutated = match kind {
            "response.output_item.added" => {
                let ev: WireOutputItemEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                self.create_slot(ev.output_index, &ev.item);
                true
            }
            "response.output_item.done" => {
                let ev: WireOutputItemEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                self.finalize_slot(ev.output_index, &ev.item)?;
                true
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let ev: WireDeltaEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                self.push_thinking(ev.output_index, &ev.delta)
            }
            "response.reasoning_summary_part.done" => {
                let ev: WireIndexEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                // The boundary between summary parts reads as a paragraph
                // break.
                self.push_thinking(ev.output_index, "\n\n")
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let ev: WireDeltaEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                self.push_text(ev.output_index, &ev.delta)
            }
            "response.function_call_arguments.delta" => {
                let ev: WireDeltaEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                match self.slots.get_mut(&ev.output_index) {
                    Some(Slot::ToolCall { args_json, .. }) => {
                        args_json.push_str(&ev.delta);
                        true
                    }
                    _ => false,
                }
            }
            "response.function_call_arguments.done" => {
                let ev: WireArgumentsDoneEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                // The done payload is authoritative; it replaces whatever
                // the deltas accumulated.
                match self.slots.get_mut(&ev.output_index) {
                    Some(Slot::ToolCall { args_json, .. }) => {
                        *args_json = ev.arguments;
                        true
                    }
                    _ => false,
                }
            }
            "response.completed" | "response.incomplete" => {
                let ev: WireResponseEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                self.finalize_response(&ev.response)?;
                true
            }
            "response.failed" => {
                let ev: WireResponseEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                self.terminal_seen = true;
                return Err(overflow::mid_stream(response_failure(&ev.response)).into());
            }
            "error" => {
                let ev: WireErrorEvent =
                    serde_json::from_value(value).map_err(ProviderError::Json)?;
                let detail = ev.message.unwrap_or_else(|| {
                    ev.code
                        .map(|c| format!("error code {c}"))
                        .unwrap_or_else(|| "unknown error".to_string())
                });
                return Err(overflow::mid_stream(detail).into());
            }
            // response.created and event kinds we do not model.
            _ => false,
        };

        if mutated {
            let _ = tx.try_send(AgentEvent::MessageUpdate {
                message: Box::new(self.current()),
            });
        }
        Ok(())
    }

    /// Open the slot for a new output item and push its block.
    fn create_slot(&mut self, output_index: usize, item: &JsonValue) {
        match item["type"].as_str() {
            Some("reasoning") => {
                self.blocks.push(ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                });
                self.slots.insert(
                    output_index,
                    Slot::Thinking {
                        block: self.blocks.len() - 1,
                    },
                );
            }
            Some("message") => {
                self.blocks.push(ContentBlock::Text {
                    text: String::new(),
                    signature: None,
                });
                self.slots.insert(
                    output_index,
                    Slot::Text {
                        block: self.blocks.len() - 1,
                    },
                );
            }
            Some("function_call") => {
                let Ok(call) = serde_json::from_value::<WireFunctionCall>(item.clone()) else {
                    return;
                };
                // The block id carries both halves of the wire identity so
                // replay can split them apart again.
                let id = match &call.id {
                    Some(item_id) => format!("{}|{item_id}", call.call_id),
                    None => call.call_id.clone(),
                };
                self.blocks.push(ContentBlock::ToolUse {
                    id,
                    name: call.name,
                    input: JsonValue::Null,
                });
                self.slots.insert(
                    output_index,
                    Slot::ToolCall {
                        block: self.blocks.len() - 1,
                        args_json: call.arguments,
                    },
                );
            }
            // Server-side items (web search, file search, ...) are not
            // modelled; their deltas address this slot and are skipped.
            _ => {}
        }
    }

    /// Close a slot on `output_item.done`: the done item is authoritative
    /// for text and arguments, and it carries the identities that become
    /// the block signatures.
    fn finalize_slot(
        &mut self,
        output_index: usize,
        item: &JsonValue,
    ) -> Result<(), anyhow::Error> {
        if !self.slots.contains_key(&output_index) {
            self.create_slot(output_index, item);
        }
        let Some(slot) = self.slots.remove(&output_index) else {
            return Ok(());
        };
        match (item["type"].as_str(), slot) {
            (Some("reasoning"), Slot::Thinking { block }) => {
                if let Some(text) = reasoning_text(item)
                    && let ContentBlock::Thinking { thinking, .. } = &mut self.blocks[block]
                {
                    *thinking = text;
                }
                // The serialized item IS the replay signature.
                if let ContentBlock::Thinking { signature, .. } = &mut self.blocks[block] {
                    *signature = Some(item.to_string());
                }
                if let Some(id) = item["id"].as_str() {
                    self.reasoning_blocks.insert(id.to_string(), block);
                }
            }
            (Some("message"), Slot::Text { block }) => {
                let msg: WireOutputMessage =
                    serde_json::from_value(item.clone()).map_err(ProviderError::Json)?;
                if !msg.content.is_empty()
                    && let ContentBlock::Text { text, .. } = &mut self.blocks[block]
                {
                    *text = msg
                        .content
                        .iter()
                        .map(|c| match c.kind.as_str() {
                            "output_text" => c.text.clone().unwrap_or_default(),
                            _ => c.refusal.clone().unwrap_or_default(),
                        })
                        .collect();
                }
                if let ContentBlock::Text { signature, .. } = &mut self.blocks[block] {
                    *signature = Some(encode_text_signature(&msg.id, msg.phase.as_deref()));
                }
            }
            (Some("function_call"), Slot::ToolCall { block, args_json }) => {
                let call: WireFunctionCall =
                    serde_json::from_value(item.clone()).map_err(ProviderError::Json)?;
                let source = if call.arguments.is_empty() {
                    args_json
                } else {
                    call.arguments
                };
                let input = parse_arguments(&source)?;
                if let ContentBlock::ToolUse {
                    input: slot_input, ..
                } = &mut self.blocks[block]
                {
                    *slot_input = input;
                }
            }
            // Item kind and slot kind disagree; keep the accumulated state.
            _ => {}
        }
        Ok(())
    }

    fn push_thinking(&mut self, output_index: usize, delta: &str) -> bool {
        match self.slots.get(&output_index) {
            Some(Slot::Thinking { block }) => {
                if let ContentBlock::Thinking { thinking, .. } = &mut self.blocks[*block] {
                    thinking.push_str(delta);
                }
                true
            }
            _ => false,
        }
    }

    fn push_text(&mut self, output_index: usize, delta: &str) -> bool {
        match self.slots.get(&output_index) {
            Some(Slot::Text { block }) => {
                if let ContentBlock::Text { text, .. } = &mut self.blocks[*block] {
                    text.push_str(delta);
                }
                true
            }
            _ => false,
        }
    }

    /// Fold a terminal response event into the message: signature backfill,
    /// usage, and the stop reason.
    fn finalize_response(&mut self, response: &WireResponse) -> Result<(), anyhow::Error> {
        self.terminal_seen = true;
        self.backfill_reasoning_signatures(&response.output);
        if let Some(usage) = &response.usage {
            self.usage = to_usage(usage);
        }
        self.stop_reason = match response.status.as_deref() {
            Some("incomplete") => Some(crate::types::StopReason::Length),
            Some("failed") | Some("cancelled") => {
                return Err(overflow::mid_stream(response_failure(response)).into());
            }
            // completed, in_progress, queued, and anything unrecognized.
            _ => Some(crate::types::StopReason::Stop),
        };
        // A response that emitted tool calls ended for tool use, whatever
        // the status says.
        if self.stop_reason == Some(crate::types::StopReason::Stop)
            && self
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        {
            self.stop_reason = Some(crate::types::StopReason::ToolUse);
        }
        Ok(())
    }

    /// Reasoning signatures captured mid-stream can lack `encrypted_content`
    /// when an endpoint only attaches it to the completed response. Merge it
    /// in so the replayed item satisfies the pairing validation.
    fn backfill_reasoning_signatures(&mut self, output: &[JsonValue]) {
        for item in output {
            if item["type"].as_str() != Some("reasoning") {
                continue;
            }
            let Some(encrypted) = item.get("encrypted_content").filter(|v| !v.is_null()) else {
                continue;
            };
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            let Some(&block) = self.reasoning_blocks.get(id) else {
                continue;
            };
            let merged = {
                let ContentBlock::Thinking {
                    signature: Some(sig),
                    ..
                } = &self.blocks[block]
                else {
                    continue;
                };
                let Ok(mut stored) = serde_json::from_str::<JsonValue>(sig) else {
                    continue;
                };
                if stored
                    .get("encrypted_content")
                    .is_some_and(|v| !v.is_null())
                {
                    continue;
                }
                stored["encrypted_content"] = encrypted.clone();
                stored.to_string()
            };
            if let ContentBlock::Thinking { signature, .. } = &mut self.blocks[block] {
                *signature = Some(merged);
            }
        }
    }

    fn finish(mut self, tx: &mpsc::Sender<AgentEvent>) -> Result<AgentMessage, anyhow::Error> {
        if !self.terminal_seen {
            return Err(ProviderError::MidStream(
                "stream ended before a terminal response event".to_string(),
            )
            .into());
        }
        // A terminal event can arrive while a slot is still open; resolve
        // its accumulated arguments now that the stream is complete.
        for (_, slot) in std::mem::take(&mut self.slots) {
            if let Slot::ToolCall { block, args_json } = slot {
                let input = parse_arguments(&args_json)?;
                if let ContentBlock::ToolUse {
                    input: slot_input, ..
                } = &mut self.blocks[block]
                {
                    *slot_input = input;
                }
            }
        }
        let message = self.current();
        let _ = tx.try_send(AgentEvent::MessageEnd {
            message: Box::new(message.clone()),
        });
        Ok(message)
    }
}

/// The accumulated argument string becomes the call's input: empty means a
/// no-argument call, malformed JSON is a stream error.
fn parse_arguments(source: &str) -> Result<JsonValue, anyhow::Error> {
    if source.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(source).map_err(|e| ProviderError::Json(e).into())
}

/// The failure detail a terminal event carries: the structured error when
/// present, else the incomplete reason, else a bare marker.
fn response_failure(response: &WireResponse) -> String {
    if let Some(error) = &response.error {
        let code = error
            .code
            .as_ref()
            .map(|c| {
                c.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| c.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());
        let message = error.message.as_deref().unwrap_or("no message");
        return format!("{code}: {message}");
    }
    if let Some(details) = &response.incomplete_details
        && let Some(reason) = &details.reason
    {
        return format!("incomplete: {reason}");
    }
    "unknown error (no error details in response)".to_string()
}

/// The display text of a completed reasoning item: the summary when the
/// server provides one, else the raw reasoning content.
fn reasoning_text(item: &JsonValue) -> Option<String> {
    let join = |key: &str| -> Option<String> {
        let parts: Vec<&str> = item[key]
            .as_array()?
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    };
    join("summary").or_else(|| join("content"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Model, StopReason, ThinkingKind};
    use serde_json::json;

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

    fn feed(
        acc: &mut Accumulator,
        tx: &mpsc::Sender<AgentEvent>,
        payload: &str,
    ) -> Result<(), anyhow::Error> {
        apply_payload(acc, payload, tx)
    }

    /// The standard terminal event.
    fn completed(usage: &str) -> String {
        format!(
            r#"{{"type":"response.completed","response":{{"id":"r1","status":"completed","usage":{usage},"output":[]}}}}"#
        )
    }

    #[test]
    fn text_stream_produces_lifecycle_events() {
        let (tx, rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m1","role":"assistant","content":[],"status":"in_progress"}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"Hello"}"#,
        )
        .unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":", world"}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"Hello, world","annotations":[]}],"status":"completed","phase":"final_answer"}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            &completed(r#"{"input_tokens":10,"output_tokens":5}"#),
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant {
            content,
            stop_reason,
            usage,
            ..
        } = &msg
        else {
            panic!("expected assistant")
        };
        assert_eq!(content.len(), 1);
        let ContentBlock::Text { text, signature } = &content[0] else {
            panic!("expected text")
        };
        assert_eq!(text, "Hello, world");
        assert!(signature.is_some());
        assert_eq!(*stop_reason, Some(StopReason::Stop));
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);

        // The signature decodes back to the item identity.
        let (id, phase) = translate::parse_text_signature(signature.as_deref().unwrap());
        assert_eq!(id, "m1");
        assert_eq!(phase.as_deref(), Some("final_answer"));

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
    fn reasoning_stream_captures_raw_item_as_signature() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"let me"}"#,
        )
        .unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#,
        )
        .unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"think"}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"let me"},{"type":"summary_text","text":"think"}],"encrypted_content":"enc1"}}"#).unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"m1","role":"assistant","content":[],"status":"in_progress"}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"answer"}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}],"status":"completed"}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            &completed(r#"{"input_tokens":1,"output_tokens":1}"#),
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(content.len(), 2);
        let ContentBlock::Thinking {
            thinking,
            signature,
        } = &content[0]
        else {
            panic!("expected thinking")
        };
        // The done item's joined summary is the final text.
        assert_eq!(thinking, "let me\n\nthink");
        // The signature is the serialized reasoning item, replayable verbatim.
        let stored: JsonValue = serde_json::from_str(signature.as_deref().unwrap()).unwrap();
        assert_eq!(stored["id"], "rs_1");
        assert_eq!(stored["type"], "reasoning");
        assert_eq!(stored["encrypted_content"], "enc1");
        assert!(matches!(&content[1], ContentBlock::Text { text, .. } if text == "answer"));
    }

    #[test]
    fn function_call_assembles_and_stops_tool_use() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"read","arguments":""}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"pa"}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"x\"}"}"#).unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"read","arguments":"{\"path\":\"x\"}"}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            &completed(r#"{"input_tokens":1,"output_tokens":1}"#),
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
        let ContentBlock::ToolUse { id, name, input } = &content[0] else {
            panic!("expected tool_use")
        };
        // The block id keeps both halves of the wire identity.
        assert_eq!(id, "call_1|fc_1");
        assert_eq!(name, "read");
        assert_eq!(*input, json!({"path": "x"}));
    }

    #[test]
    fn done_item_arguments_win_over_deltas() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c","id":"fc_1","name":"f","arguments":""}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{bad"}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","id":"fc_1","name":"f","arguments":"{\"a\":1}"}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            &completed(r#"{"input_tokens":1,"output_tokens":1}"#),
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert!(
            matches!(&content[0], ContentBlock::ToolUse { input, .. } if *input == json!({"a": 1}))
        );
    }

    #[test]
    fn incomplete_maps_to_max_tokens() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant","content":[]}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"hi"}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.incomplete","response":{"id":"r","status":"incomplete","usage":{"input_tokens":1,"output_tokens":1},"output":[]}}"#).unwrap();
        let msg = acc.finish(&tx).unwrap();
        let AgentMessage::Assistant { stop_reason, .. } = &msg else {
            panic!("expected assistant")
        };
        assert_eq!(*stop_reason, Some(StopReason::Length));
    }

    #[test]
    fn failed_event_becomes_midstream_error() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let err = feed(
            &mut acc,
            &tx,
            r#"{"type":"response.failed","response":{"id":"r","status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
        )
        .unwrap_err();
        let e = err.downcast_ref::<ProviderError>().expect("ProviderError");
        assert!(matches!(e, ProviderError::MidStream(m) if m == "server_error: boom"));
    }

    #[test]
    fn error_event_becomes_midstream_error() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let err = feed(
            &mut acc,
            &tx,
            r#"{"type":"error","code":"rate_limit_exceeded","message":"slow down"}"#,
        )
        .unwrap_err();
        let e = err.downcast_ref::<ProviderError>().expect("ProviderError");
        assert!(matches!(e, ProviderError::MidStream(m) if m == "slow down"));
    }

    #[test]
    fn bare_error_payload_and_typeless_payloads() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let err = feed(
            &mut acc,
            &tx,
            r#"{"error":{"message":"boom","type":"server_error"}}"#,
        )
        .unwrap_err();
        let e = err.downcast_ref::<ProviderError>().expect("ProviderError");
        assert!(matches!(e, ProviderError::MidStream(m) if m == "boom"));

        // An HTTP error envelope (code+message, no event type) streamed as
        // data on a 2xx response is also a mid-stream error.
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        let err = feed(
            &mut acc,
            &tx,
            r#"{"code":"InvalidParameter","message":"Unsupported model: 'x'.","request_id":"r1"}"#,
        )
        .unwrap_err();
        let e = err.downcast_ref::<ProviderError>().expect("ProviderError");
        assert!(matches!(e, ProviderError::MidStream(m) if m == "Unsupported model: 'x'."));

        // A payload without a type and without an error is ignored.
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        feed(&mut acc, &tx, r#"{"some":"vendor extension"}"#).unwrap();
    }

    #[test]
    fn missing_terminal_event_is_an_error() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());
        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant","content":[]}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"hi"}"#,
        )
        .unwrap();
        let err = acc.finish(&tx).unwrap_err();
        let e = err.downcast_ref::<ProviderError>().expect("ProviderError");
        assert!(matches!(e, ProviderError::MidStream(m) if m.contains("terminal")));
    }

    #[test]
    fn encrypted_content_backfilled_from_completed_response() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        // The done item lacks encrypted_content (the Azure behavior).
        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"hmm"}"#,
        )
        .unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"hmm"}]}}"#).unwrap();
        // The completed response carries it.
        feed(&mut acc, &tx, r#"{"type":"response.completed","response":{"id":"r","status":"completed","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"hmm"}],"encrypted_content":"enc-late"}]}}"#).unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        let ContentBlock::Thinking { signature, .. } = &content[0] else {
            panic!("expected thinking")
        };
        let stored: JsonValue = serde_json::from_str(signature.as_deref().unwrap()).unwrap();
        assert_eq!(stored["encrypted_content"], "enc-late");
    }

    #[test]
    fn leftover_tool_call_slot_resolves_at_finish() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        // A terminal event can arrive before output_item.done.
        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c","id":"fc_1","name":"f","arguments":""}}"#).unwrap();
        feed(&mut acc, &tx, r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"a\":2}"}"#).unwrap();
        feed(
            &mut acc,
            &tx,
            &completed(r#"{"input_tokens":1,"output_tokens":1}"#),
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert!(
            matches!(&content[0], ContentBlock::ToolUse { input, .. } if *input == json!({"a": 2}))
        );
    }

    #[test]
    fn unknown_events_and_server_side_items_are_ignored() {
        let (tx, _rx) = chan();
        let mut acc = Accumulator::new(&ctx());

        feed(&mut acc, &tx, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}"#).unwrap();
        // Deltas addressing the unmodelled slot must not create blocks.
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"stray"}"#,
        )
        .unwrap();
        feed(
            &mut acc,
            &tx,
            r#"{"type":"response.web_search_call.searching","output_index":0}"#,
        )
        .unwrap();
        feed(
            &mut acc,
            &tx,
            &completed(r#"{"input_tokens":1,"output_tokens":1}"#),
        )
        .unwrap();
        let msg = acc.finish(&tx).unwrap();

        let AgentMessage::Assistant { content, .. } = &msg else {
            panic!("expected assistant")
        };
        assert!(content.is_empty());
    }
}
