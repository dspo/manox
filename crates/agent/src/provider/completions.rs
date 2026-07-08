//! `LanguageModel` implementation for the OpenAI Chat Completions wire.
//!
//! - Request: POST `{endpoint}/chat/completions`, `Authorization: Bearer`, body
//!   `{model, messages, max_tokens, stream:true, tools}`.
//! - Response: SSE `data:` lines, each `{"choices":[{"delta":{"content|tool_calls|...},"finish_reason":...}]}`,
//!   terminating with `data: [DONE]`.
//!
//! Tool calls flow inside the assistant message's `tool_calls` array; the
//! mapper tracks per-`index` state to stitch streamed argument fragments into
//! complete `LanguageModelToolUse` events.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    LanguageModelToolUse, MessageContent, Role, StopReason,
};
use crate::provider::sse::{extract_data_line, fix_streamed_json};

pub struct CompletionsModel {
    id: String,
    name: String,
    provider_name: String,
    api_model_id: String,
    endpoint_url: String,
    api_key: String,
    max_output_tokens: u64,
    max_token_count: u64,
    /// Whether the endpoint is official OpenAI (api.openai.com) and thus
    /// eligible for `prompt_cache_retention:"24h"`.
    long_ttl: bool,
}

impl CompletionsModel {
    pub fn new(
        id: String,
        name: String,
        provider_name: String,
        api_model_id: String,
        endpoint_url: String,
        api_key: String,
        max_token_count: u64,
    ) -> Self {
        Self {
            id,
            name,
            provider_name,
            api_model_id,
            endpoint_url: endpoint_url.clone(),
            api_key,
            max_output_tokens: max_token_count.min(8192),
            max_token_count,
            long_ttl: crate::provider::openai_long_ttl(&endpoint_url),
        }
    }
}

impl LanguageModel for CompletionsModel {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn provider_id(&self) -> String {
        format!("completions:{}", self.provider_name)
    }
    fn provider_name(&self) -> String {
        self.provider_name.clone()
    }
    fn wire_api(&self) -> crate::provider::WireApi {
        crate::provider::WireApi::Completions
    }
    fn max_token_count(&self) -> u64 {
        self.max_token_count
    }
    fn supports_long_prompt_cache_retention(&self) -> bool {
        self.long_ttl
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        _cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<LanguageModelCompletionEvent>>>> {
        let url = completions_url(&self.endpoint_url);
        let api_key = self.api_key.clone();
        let model = self.api_model_id.clone();
        let max_tokens = self.max_output_tokens;
        let prompt_cache_key = self.id.clone();
        let long_ttl = self.long_ttl;

        Box::pin(async move {
            let (tx, rx) = async_channel::bounded::<Result<LanguageModelCompletionEvent>>(64);
            let tx_clone = tx.clone();
            crate::runtime::handle().spawn(async move {
                if let Err(e) = stream_completions(
                    &url,
                    &api_key,
                    &model,
                    max_tokens,
                    request,
                    tx_clone,
                    &prompt_cache_key,
                    long_ttl,
                )
                .await
                {
                    let _ = tx.send(Err(e)).await;
                }
            });
            let stream: BoxStream<'static, Result<LanguageModelCompletionEvent>> = Box::pin(rx);
            Ok(stream)
        })
    }
}

fn completions_url(endpoint: &str) -> String {
    if endpoint.ends_with("/chat/completions") {
        endpoint.to_string()
    } else {
        format!("{}/chat/completions", endpoint.trim_end_matches('/'))
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn stream_completions(
    url: &str,
    api_key: &str,
    model: &str,
    max_tokens: u64,
    request: LanguageModelRequest,
    tx: async_channel::Sender<Result<LanguageModelCompletionEvent>>,
    prompt_cache_key: &str,
    long_ttl: bool,
) -> Result<()> {
    if crate::provider::is_deepseek(url, model) {
        tracing::debug!(
            target: "provider",
            "deepseek endpoint detected at {url}: reasoning_content + cache telemetry parsing active"
        );
    }
    let body = build_request_body(model, max_tokens, &request, prompt_cache_key, long_ttl);
    let client = reqwest::Client::builder()
        .build()
        .context("Failed to build reqwest client")?;

    // Retry the handshake (429 / 5xx / network errors) before any SSE event is
    // forwarded. The body is shared by reference so every attempt sends the
    // byte-identical request — provider-side prompt caching is unaffected.
    let url_owned = url.to_string();
    let api_key_owned = api_key.to_string();
    let response = match crate::provider::retry::send_with_retry(
        || {
            let client = client.clone();
            client
                .post(&url_owned)
                .header("Content-Type", "application/json")
                .bearer_auth(&api_key_owned)
                .json(&body)
                .send()
        },
        &tx,
        "Completions API",
    )
    .await
    {
        Ok(resp) => resp,
        // Terminal failure or cancellation — the error has already been
        // forwarded through `tx` (or the receiver was dropped). Stop the stream
        // without emitting a second error.
        Err(_) => return Ok(()),
    };

    let mut mapper = CompletionsEventMapper::new();
    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read SSE chunk")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();
            let Some(data) = extract_data_line(&line) else {
                continue;
            };
            if data == "[DONE]" {
                flush_and_stop(&mut mapper, &tx).await;
                return Ok(());
            }
            let value: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx
                        .send(Err(anyhow!("Failed to parse SSE event: {e}")))
                        .await;
                    continue;
                }
            };
            for event in mapper.map_chunk(&value) {
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    flush_and_stop(&mut mapper, &tx).await;
    Ok(())
}

async fn flush_and_stop(
    mapper: &mut CompletionsEventMapper,
    tx: &async_channel::Sender<Result<LanguageModelCompletionEvent>>,
) {
    for event in mapper.flush_pending() {
        if tx.send(Ok(event)).await.is_err() {
            return;
        }
    }
    if !mapper.stop_emitted() {
        let _ = tx
            .send(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)))
            .await;
    }
}

fn build_request_body(
    model: &str,
    max_tokens: u64,
    request: &LanguageModelRequest,
    prompt_cache_key: &str,
    long_ttl: bool,
) -> Value {
    let messages = messages_to_openai(&request.messages);
    // Chat Completions has no first-party `prompt_cache_key`, but many
    // OpenAI-compatible servers (OpenRouter/vLLM) accept it. Sending it is a
    // harmless no-op where unsupported and lets compatible servers reuse the
    // cached prefix across turns.
    let cache_key = crate::provider::clamp_prompt_cache_key(prompt_cache_key);
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
        "prompt_cache_key": cache_key,
    });
    if long_ttl {
        body["prompt_cache_retention"] = Value::String("24h".to_string());
    }
    if let Some(effort) = request.reasoning_effort {
        body["reasoning_effort"] = Value::String(effort.openai_wire_value(long_ttl).to_string());
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
        );
    }
    body
}

/// Chat Completions wire input: tool calls live inside assistant `tool_calls` arrays; tool results become separate `role:"tool"` messages keyed by `tool_call_id`.
fn messages_to_openai(messages: &[LanguageModelRequestMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        let mut text_buf = String::new();
        let mut tool_uses: Vec<&crate::language_model::LanguageModelToolUse> = Vec::new();
        let mut tool_results: Vec<&crate::language_model::LanguageModelToolResult> = Vec::new();
        let mut images: Vec<(&str, &str)> = Vec::new();

        for c in &m.content {
            match c {
                MessageContent::Text(t) => text_buf.push_str(t),
                MessageContent::Thinking { text, .. } => text_buf.push_str(text),
                MessageContent::ToolUse(tu) => tool_uses.push(tu),
                MessageContent::ToolResult(tr) => tool_results.push(tr),
                MessageContent::Image { data, mime_type } => images.push((data, mime_type)),
            }
        }

        let has_text = !text_buf.trim().is_empty();
        let has_tool_uses = !tool_uses.is_empty();
        let has_tool_results = !tool_results.is_empty();
        let has_images = !images.is_empty();

        if !has_text && !has_tool_uses && !has_tool_results && !has_images {
            continue;
        }

        match m.role {
            // Chat Completions wire carries the system prompt as a `system` role
            // message — the standard slot for this API. (Responses wire lifts it
            // to the top-level `instructions` field instead; see responses.rs.)
            Role::System => {
                out.push(json!({"role": "system", "content": text_buf}));
            }
            Role::User => {
                if has_images {
                    // Multimodal user turn: `content` becomes a parts array mixing
                    // one text part (if any) with `image_url` parts as data URLs.
                    let mut parts: Vec<Value> = Vec::new();
                    if has_text {
                        parts.push(json!({"type": "text", "text": text_buf}));
                    }
                    for (data, mime) in &images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{mime};base64,{data}")},
                        }));
                    }
                    out.push(json!({"role": "user", "content": parts}));
                } else if has_text {
                    out.push(json!({"role": "user", "content": text_buf}));
                }
                for tr in tool_results {
                    // DashScope Chat Completions endpoint rejects `is_error`
                    // on some models; fold the error bit into the content.
                    let content = if tr.is_error {
                        format!("[error] {}", tr.content)
                    } else {
                        tr.content.clone()
                    };
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": tr.tool_use_id,
                        "content": content,
                    }));
                }
            }
            Role::Assistant => {
                if has_tool_uses {
                    let tool_calls: Vec<Value> = tool_uses
                        .iter()
                        .map(|tu| {
                            let arguments = if !tu.raw_input.is_empty() {
                                tu.raw_input.clone()
                            } else {
                                tu.input.to_string()
                            };
                            json!({
                                "id": tu.id,
                                "type": "function",
                                "function": {
                                    "name": tu.name,
                                    "arguments": arguments,
                                }
                            })
                        })
                        .collect();
                    let mut msg = json!({"role": "assistant", "tool_calls": tool_calls});
                    if has_text {
                        msg["content"] = Value::String(text_buf);
                    } else {
                        msg["content"] = Value::Null;
                    }
                    out.push(msg);
                } else if has_text {
                    out.push(json!({"role": "assistant", "content": text_buf}));
                }
            }
        }
    }
    out
}

#[derive(Debug, Default, Deserialize)]
struct CompletionsChunk {
    #[serde(default)]
    choices: Vec<CompletionsChoice>,
    /// DeepSeek (and other OpenAI-compat reasoning endpoints) report token
    /// usage on the final chunk. Parsed unconditionally as an optional field —
    /// present-when-relevant, harmless when absent, shared with other
    /// OpenAI-compatible reasoning models.
    #[serde(default)]
    usage: Option<CompletionsUsage>,
}

/// Token-usage payload for the completions wire. DeepSeek extends the standard
/// OpenAI usage with `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`,
/// surfaced to the UI as cache-read / cache-creation counts. The standard
/// fields are kept for parity even though manox currently only consumes the
/// cache columns.
#[derive(Debug, Default, Deserialize)]
struct CompletionsUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionsChoice {
    #[serde(default)]
    delta: CompletionsDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionsDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek-V3/V4 reasoning models stream the thinking trace in this field
    /// (parallel to `content`). Parsed unconditionally; absent on non-reasoning
    /// models. Mapped to `Thinking` so the existing reasoning-block UI renders it.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

struct RawToolUse {
    id: String,
    name: Arc<str>,
    input_json: String,
}

struct CompletionsEventMapper {
    tool_uses_by_index: HashMap<usize, RawToolUse>,
    finished_call_ids: HashSet<String>,
    stop_reason: StopReason,
    stop_emitted: bool,
}

impl CompletionsEventMapper {
    fn new() -> Self {
        Self {
            tool_uses_by_index: HashMap::new(),
            finished_call_ids: HashSet::new(),
            stop_reason: StopReason::EndTurn,
            stop_emitted: false,
        }
    }

    fn stop_emitted(&self) -> bool {
        self.stop_emitted
    }

    fn map_chunk(&mut self, value: &Value) -> Vec<Result<LanguageModelCompletionEvent>> {
        let Ok(chunk) = serde_json::from_value::<CompletionsChunk>(value.clone()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                out.push(Ok(LanguageModelCompletionEvent::Text(content)));
            }
            // DeepSeek reasoning models stream the thinking trace in
            // `reasoning_content`; map it to `Thinking` so the existing
            // reasoning-block UI renders it (previously discarded entirely).
            if let Some(reasoning) = choice.delta.reasoning_content
                && !reasoning.is_empty()
            {
                out.push(Ok(LanguageModelCompletionEvent::Thinking {
                    text: reasoning,
                    signature: None,
                }));
            }
            if let Some(tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    if let Some(event) = self.apply_tool_call_delta(tc) {
                        out.push(Ok(event));
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.stop_reason = match reason.as_str() {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::MaxTokens,
                    "tool_calls" => StopReason::ToolUse,
                    "content_filter" => StopReason::Refusal,
                    _ => StopReason::EndTurn,
                };
                // Flush any pending tool_use to is_input_complete=true at finish.
                for event in self.complete_all_pending() {
                    out.push(Ok(event));
                }
                out.push(Ok(LanguageModelCompletionEvent::Stop(self.stop_reason)));
                self.stop_emitted = true;
            }
        }
        // DeepSeek reports cache hit/miss token counts on the final chunk;
        // surface them so the UI's `cache: NN%` chip can cross-check the
        // stability ratio against real cache hits. Standard OpenAI-compat
        // endpoints without these fields simply emit nothing extra.
        if let Some(usage) = chunk.usage {
            out.push(Ok(LanguageModelCompletionEvent::UsageUpdate(
                crate::language_model::TokenUsage {
                    input_tokens: usage.prompt_tokens.unwrap_or(0),
                    output_tokens: usage.completion_tokens.unwrap_or(0),
                    cache_read_input_tokens: usage.prompt_cache_hit_tokens.unwrap_or(0),
                    cache_creation_input_tokens: usage.prompt_cache_miss_tokens.unwrap_or(0),
                },
            )));
        }
        out
    }

    fn apply_tool_call_delta(&mut self, tc: DeltaToolCall) -> Option<LanguageModelCompletionEvent> {
        let slot = self
            .tool_uses_by_index
            .entry(tc.index)
            .or_insert_with(|| RawToolUse {
                id: String::new(),
                name: Arc::from(""),
                input_json: String::new(),
            });
        if let Some(id) = tc.id {
            slot.id = id;
        }
        if let Some(func) = tc.function {
            if let Some(name) = func.name {
                slot.name = Arc::from(name);
            }
            if let Some(args) = func.arguments {
                slot.input_json.push_str(&args);
            }
        }
        if slot.id.is_empty() {
            return None;
        }
        Self::try_emit_partial(slot)
    }

    fn try_emit_partial(slot: &RawToolUse) -> Option<LanguageModelCompletionEvent> {
        let input = fix_streamed_json(&slot.input_json).ok()?;
        Some(LanguageModelCompletionEvent::ToolUse(
            LanguageModelToolUse {
                id: slot.id.clone(),
                name: slot.name.clone(),
                is_input_complete: false,
                raw_input: slot.input_json.clone(),
                input,
                thought_signature: None,
            },
        ))
    }

    fn complete_all_pending(&mut self) -> Vec<LanguageModelCompletionEvent> {
        let indices: Vec<usize> = self.tool_uses_by_index.keys().copied().collect();
        let mut out = Vec::new();
        for idx in indices {
            if let Some(mut slot) = self.tool_uses_by_index.remove(&idx)
                && let Some(ev) = self.emit_complete(&mut slot)
            {
                out.push(ev);
            }
        }
        out
    }

    fn emit_complete(&mut self, slot: &mut RawToolUse) -> Option<LanguageModelCompletionEvent> {
        if slot.id.is_empty() {
            return None;
        }
        if !self.finished_call_ids.insert(slot.id.clone()) {
            return None;
        }
        let input = match fix_streamed_json(&slot.input_json) {
            Ok(v) => v,
            Err(e) => {
                return Some(LanguageModelCompletionEvent::ToolUseJsonParseError {
                    id: slot.id.clone(),
                    tool_name: slot.name.clone(),
                    raw_input: slot.input_json.clone(),
                    json_parse_error: e.to_string(),
                });
            }
        };
        Some(LanguageModelCompletionEvent::ToolUse(
            LanguageModelToolUse {
                id: slot.id.clone(),
                name: slot.name.clone(),
                is_input_complete: true,
                raw_input: slot.input_json.clone(),
                input,
                thought_signature: None,
            },
        ))
    }

    fn flush_pending(&mut self) -> Vec<LanguageModelCompletionEvent> {
        self.complete_all_pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{
        LanguageModelRequestMessage, LanguageModelRequestTool, LanguageModelToolResult,
        ReasoningEffort,
    };
    use crate::provider::WireApi;

    fn req_with_tool() -> LanguageModelRequest {
        LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text("hi".to_string())],
                cache: false,
            }],
            tools: vec![LanguageModelRequestTool {
                name: "bash".to_string(),
                description: "run a shell command".to_string(),
                input_schema: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
                use_input_streaming: false,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn build_request_body_includes_tools() {
        let body = build_request_body("m", 64, &req_with_tool(), "test-key", false);
        let tools = body.get("tools").and_then(Value::as_array).expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "bash");
        assert!(tools[0]["function"]["description"].is_string());
        assert!(tools[0]["function"]["parameters"].is_object());
    }

    #[test]
    fn build_request_body_includes_reasoning_effort() {
        let mut req = req_with_tool();
        req.reasoning_effort = Some(ReasoningEffort::Max);
        let body = build_request_body("m", 64, &req, "test-key", false);
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn build_request_body_clamps_reasoning_effort_for_official_openai() {
        let mut req = req_with_tool();
        req.reasoning_effort = Some(ReasoningEffort::Ultracode);
        let body = build_request_body("m", 64, &req, "test-key", true);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn build_request_body_passes_auto_effort_to_official_openai() {
        let mut req = req_with_tool();
        req.reasoning_effort = Some(ReasoningEffort::Auto);
        let body = build_request_body("m", 64, &req, "test-key", true);
        assert_eq!(body["reasoning_effort"], "auto");
    }

    #[test]
    fn messages_to_openai_emits_tool_calls_and_tool_role() {
        let tu = LanguageModelToolUse {
            id: "call_1".to_string(),
            name: Arc::from("bash"),
            raw_input: r#"{"cmd":"ls"}"#.to_string(),
            input: json!({"cmd": "ls"}),
            is_input_complete: true,
            thought_signature: None,
        };
        let tr = LanguageModelToolResult {
            tool_use_id: "call_1".to_string(),
            tool_name: Arc::from("bash"),
            is_error: false,
            content: "file.rs".to_string(),
        };
        let messages = vec![
            LanguageModelRequestMessage {
                role: Role::Assistant,
                content: vec![MessageContent::ToolUse(tu)],
                cache: false,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::ToolResult(tr)],
                cache: false,
            },
        ];
        let out = messages_to_openai(&messages);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[0]["tool_calls"][0]["type"], "function");
        assert_eq!(out[0]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            out[0]["tool_calls"][0]["function"]["arguments"],
            r#"{"cmd":"ls"}"#
        );
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["tool_call_id"], "call_1");
        assert_eq!(out[1]["content"], "file.rs");
    }

    #[test]
    fn messages_to_openai_keeps_plain_text() {
        let messages = vec![LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text("hello".to_string())],
            cache: false,
        }];
        let out = messages_to_openai(&messages);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"], "hello");
    }

    fn make_tool_delta(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> Value {
        let mut function = serde_json::Map::new();
        if let Some(n) = name {
            function.insert("name".to_string(), Value::String(n.to_string()));
        }
        if let Some(a) = args {
            function.insert("arguments".to_string(), Value::String(a.to_string()));
        }
        let mut tc = serde_json::Map::new();
        tc.insert("index".to_string(), Value::Number(index.into()));
        if let Some(id) = id {
            tc.insert("id".to_string(), Value::String(id.to_string()));
        }
        tc.insert("function".to_string(), Value::Object(function));
        json!({
            "choices": [{
                "delta": {
                    "tool_calls": [Value::Object(tc)]
                }
            }]
        })
    }

    #[test]
    fn map_chunk_assembles_streamed_tool_input() {
        let mut m = CompletionsEventMapper::new();
        let events = vec![
            make_tool_delta(0, Some("call_1"), Some("bash"), None),
            make_tool_delta(0, None, None, Some(r#"{"cmd":"#)),
            make_tool_delta(0, None, None, Some(r#""ls"}"#)),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
        ];
        let mut all = Vec::new();
        for ev in &events {
            all.extend(m.map_chunk(ev));
        }
        let tools: Vec<_> = all
            .iter()
            .filter_map(|r| match r {
                Ok(LanguageModelCompletionEvent::ToolUse(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(tools.iter().any(|t| !t.is_input_complete));
        let finals: Vec<_> = tools.iter().filter(|t| t.is_input_complete).collect();
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].id, "call_1");
        assert_eq!(&*finals[0].name, "bash");
        assert_eq!(finals[0].input["cmd"], "ls");
        let stop = all.iter().find_map(|r| match r {
            Ok(LanguageModelCompletionEvent::Stop(s)) => Some(*s),
            _ => None,
        });
        assert_eq!(stop, Some(StopReason::ToolUse));
    }

    /// Live streaming test: send "hi" via the Bailian qwen3.7-plus completions wire.
    #[tokio::test]
    async fn live_completions_stream() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return;
        }
        let config = crate::provider::CxConfig::load_default().expect("load config");
        let model = config
            .resolve_all_models()
            .into_iter()
            .find(|m| {
                m.provider_name == "百炼"
                    && m.id.contains("qwen3.7-plus")
                    && m.wire_api == WireApi::Completions
            })
            .expect("应含百炼 qwen3.7-plus completions");
        let api_key = crate::provider::resolve_apikey(
            model
                .apikey_source
                .as_deref()
                .unwrap_or("env:DASHSCOPE_API_KEY"),
        )
        .expect("resolve api key");

        let (tx, rx) = async_channel::bounded(64);
        let tx_clone = tx.clone();
        let url = completions_url(&model.endpoint_url);
        let api_model = model.api_model_id();
        let mut request = LanguageModelRequest::default();
        request.messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text("hi".to_string())],
            cache: false,
        });
        tokio::spawn(async move {
            if let Err(e) = stream_completions(
                &url, &api_key, &api_model, 64, request, tx_clone, "test-key", false,
            )
            .await
            {
                let _ = tx.send(Err(e)).await;
            }
        });

        let mut texts = 0u32;
        let mut stopped = false;
        while let Ok(ev) = rx.recv().await {
            match ev {
                Ok(LanguageModelCompletionEvent::Text(_)) => texts += 1,
                Ok(LanguageModelCompletionEvent::Stop(_)) => {
                    stopped = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("stream error: {e}"),
            }
        }
        assert!(texts > 0, "应至少收到一个 Text 事件");
        assert!(stopped, "应收到 Stop 事件");
    }

    /// DeepSeek reasoning models stream the thinking trace in
    /// `reasoning_content`; it must surface as a `Thinking` event so the
    /// existing reasoning-block UI renders it (previously discarded).
    #[test]
    fn map_chunk_emits_thinking_for_reasoning_content() {
        let mut mapper = CompletionsEventMapper::new();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": { "reasoning_content": "let me think" },
                "finish_reason": null
            }]
        });
        let events = mapper.map_chunk(&chunk);
        assert!(events.iter().any(|e| matches!(
            e,
            Ok(LanguageModelCompletionEvent::Thinking { text, signature: None }) if text == "let me think"
        )), "reasoning_content must map to Thinking: {events:?}");
    }

    /// DeepSeek's `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` must
    /// surface as a `UsageUpdate` with the cache columns populated so the UI's
    /// cache chip can cross-check the stability ratio against real hits.
    #[test]
    fn map_chunk_emits_usage_update_with_deepseek_cache_tokens() {
        let mut mapper = CompletionsEventMapper::new();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_cache_hit_tokens": 80,
                "prompt_cache_miss_tokens": 20
            }
        });
        let events = mapper.map_chunk(&chunk);
        let usage = events
            .iter()
            .find_map(|e| match e {
                Ok(LanguageModelCompletionEvent::UsageUpdate(u)) => Some(*u),
                _ => None,
            })
            .expect("UsageUpdate emitted");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 80);
        assert_eq!(usage.cache_creation_input_tokens, 20);
    }

    /// Endpoints that omit the `usage` field (standard OpenAI completions) must
    /// not emit a spurious zero-UsageUpdate.
    #[test]
    fn map_chunk_omits_usage_when_absent() {
        let mut mapper = CompletionsEventMapper::new();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": { "content": "hi" },
                "finish_reason": "stop"
            }]
        });
        let events = mapper.map_chunk(&chunk);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(LanguageModelCompletionEvent::UsageUpdate(_)))),
            "no UsageUpdate when usage absent: {events:?}"
        );
    }
}
