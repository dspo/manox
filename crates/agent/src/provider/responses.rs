//! `LanguageModel` implementation for the OpenAI Responses wire.
//!
//! - Request: POST `{endpoint}/responses`, `Authorization: Bearer`, body
//!   `{model, input, max_output_tokens, stream:true, tools}`.
//! - Response: SSE event stream; key event types:
//!   - `response.output_text.delta` → text delta
//!   - `response.output_item.added` / `response.output_item.done` → item lifecycle
//!   - `response.function_call_arguments.delta` / `.done` → tool-input streaming
//!   - `response.completed` → done
//!   - `response.failed` / `error` → error
//!
//! Tool calls flow as separate `function_call` items in the response output;
//! the mapper tracks per-`output_index` state to stitch streamed argument
//! fragments into complete `LanguageModelToolUse` events.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    LanguageModelToolUse, MessageContent, Role, StopReason, TokenUsage,
};
use crate::provider::sse::{extract_data_line, fix_streamed_json};

pub struct ResponsesModel {
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

impl ResponsesModel {
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

impl LanguageModel for ResponsesModel {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn provider_id(&self) -> String {
        format!("responses:{}", self.provider_name)
    }
    fn provider_name(&self) -> String {
        self.provider_name.clone()
    }
    fn wire_api(&self) -> crate::provider::WireApi {
        crate::provider::WireApi::Responses
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
        let url = responses_url(&self.endpoint_url);
        let api_key = self.api_key.clone();
        let model = self.api_model_id.clone();
        let max_tokens = self.max_output_tokens;
        let prompt_cache_key = self.id.clone();
        let long_ttl = self.long_ttl;

        Box::pin(async move {
            let (tx, rx) = async_channel::bounded::<Result<LanguageModelCompletionEvent>>(64);
            let tx_clone = tx.clone();
            crate::runtime::handle().spawn(async move {
                if let Err(e) = stream_responses(
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

fn responses_url(endpoint: &str) -> String {
    if endpoint.ends_with("/responses") {
        endpoint.to_string()
    } else {
        format!("{}/responses", endpoint.trim_end_matches('/'))
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn stream_responses(
    url: &str,
    api_key: &str,
    model: &str,
    max_tokens: u64,
    request: LanguageModelRequest,
    tx: async_channel::Sender<Result<LanguageModelCompletionEvent>>,
    prompt_cache_key: &str,
    long_ttl: bool,
) -> Result<()> {
    let body = build_request_body(model, max_tokens, &request, prompt_cache_key, long_ttl);
    let client = reqwest::Client::builder()
        .build()
        .context("构建 reqwest client 失败")?;

    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context("调用 Responses API 失败")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Responses API 返回 {status}: {body}"));
    }

    let mut mapper = ResponsesEventMapper::new();
    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("读取 SSE chunk 失败")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();
            let Some(data) = extract_data_line(&line) else {
                continue;
            };
            let value: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("解析 SSE 事件失败: {e}"))).await;
                    continue;
                }
            };
            for mapped in mapper.map_event(&value) {
                if tx.send(mapped).await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    // Flush any tool_use that received deltas but never received `.done` nor `output_item.done`.
    for pending in mapper.flush_pending() {
        if tx.send(Ok(pending)).await.is_err() {
            return Ok(());
        }
    }
    if !mapper.stop_emitted()
        && tx
            .send(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)))
            .await
            .is_err()
    {
        return Ok(());
    }
    Ok(())
}

fn build_request_body(
    model: &str,
    max_tokens: u64,
    request: &LanguageModelRequest,
    prompt_cache_key: &str,
    long_ttl: bool,
) -> Value {
    let (input, instructions) = build_input(&request.messages);
    // The prompt cache key is the model's stable id (`provider/model/wire`),
    // truncated to OpenAI's 64-char limit so it stays stable across turns of
    // the same model — letting the provider reuse the cached prefix.
    let cache_key = crate::provider::clamp_prompt_cache_key(prompt_cache_key);
    let mut body = json!({
        "model": model,
        "input": input,
        "max_output_tokens": max_tokens,
        "stream": true,
        // Disable server-side response storage (manox keeps its own history);
        // prompt caching still works via prefix matching on `prompt_cache_key`.
        "store": false,
        "prompt_cache_key": cache_key,
    });
    if long_ttl {
        // 24h retention only on official OpenAI endpoints.
        body["prompt_cache_retention"] = Value::String("24h".to_string());
    }
    if let Some(effort) = request.reasoning_effort {
        body["reasoning"] = json!({
            "effort": effort.openai_wire_value(long_ttl),
        });
    }
    // System prompt (e.g. a sub-agent's) goes to the top-level `instructions`
    // field — the OpenAI Responses-recommended slot — rather than a `system`
    // role message item. Anthropic wire lifts it to `system` the same way.
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    })
                })
                .collect(),
        );
    }
    body
}

/// Responses-wire input is a flat list of `message` / `function_call` / `function_call_output` items.
/// Tool calls and their results are emitted as separate top-level items per the wire spec.
/// Returns the input items plus any accumulated system-prompt text, which the caller
/// lifts to the top-level `instructions` field instead of a `system` role message item.
fn build_input(messages: &[LanguageModelRequestMessage]) -> (Vec<Value>, String) {
    let mut out: Vec<Value> = Vec::new();
    let mut system_instructions = String::new();
    for m in messages {
        let mut text_buf = String::new();
        let mut tool_uses: Vec<&LanguageModelToolUse> = Vec::new();
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
        // Images only attach to user turns; the Responses wire has no assistant image part.
        let has_images = !images.is_empty() && m.role == Role::User;

        // System messages carry the (sub-)agent's system prompt; lift their text to
        // the top-level `instructions` field rather than emitting a `system` role
        // message item. A system message is pure text by construction.
        if m.role == Role::System {
            if has_text {
                if !system_instructions.is_empty() {
                    system_instructions.push_str("\n\n");
                }
                system_instructions.push_str(&text_buf);
            }
            continue;
        }

        if has_text || has_images {
            let (role, text_kind) = match m.role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
                Role::System => unreachable!("system messages are lifted to `instructions` above"),
            };
            let mut content: Vec<Value> = Vec::new();
            if has_text {
                content.push(json!({"type": text_kind, "text": text_buf}));
            }
            for (data, mime) in &images {
                content.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime};base64,{data}"),
                }));
            }
            out.push(json!({
                "type": "message",
                "role": role,
                "content": content,
            }));
        }

        for tu in tool_uses {
            let arguments = if !tu.raw_input.is_empty() {
                tu.raw_input.clone()
            } else {
                tu.input.to_string()
            };
            out.push(json!({
                "type": "function_call",
                "call_id": tu.id,
                "name": tu.name,
                "arguments": arguments,
            }));
        }

        for tr in tool_results {
            // DashScope Responses endpoint rejects `is_error` on function_call_output;
            // fold the error bit into the output string instead (matches how kimi
            // compatibility is handled in other openai-compat providers).
            let output = if tr.is_error {
                format!("[error] {}", tr.content)
            } else {
                tr.content.clone()
            };
            out.push(json!({
                "type": "function_call_output",
                "call_id": tr.tool_use_id,
                "output": output,
            }));
        }

        // Skip a message that contributed nothing (e.g. an assistant message
        // whose only content was a single empty-text block).
        if !has_text && !has_tool_uses && !has_tool_results {
            continue;
        }
    }
    (out, system_instructions)
}

#[derive(Debug, Default, Deserialize)]
struct ResponsesEvent {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    response: Option<Value>,
    #[serde(default)]
    item: Option<Value>,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    arguments: Option<String>,
}

struct RawToolUse {
    id: String,
    name: Arc<str>,
    input_json: String,
}

struct ResponsesEventMapper {
    tool_uses_by_index: HashMap<usize, RawToolUse>,
    finished_call_ids: HashSet<String>,
    stop_reason: StopReason,
    stop_emitted: bool,
    usage: TokenUsage,
}

impl ResponsesEventMapper {
    fn new() -> Self {
        Self {
            tool_uses_by_index: HashMap::new(),
            finished_call_ids: HashSet::new(),
            stop_reason: StopReason::EndTurn,
            stop_emitted: false,
            usage: TokenUsage::default(),
        }
    }

    fn stop_emitted(&self) -> bool {
        self.stop_emitted
    }

    fn map_event(&mut self, value: &Value) -> Vec<Result<LanguageModelCompletionEvent>> {
        let Ok(ev) = serde_json::from_value::<ResponsesEvent>(value.clone()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match ev.ty.as_str() {
            "response.output_text.delta" => {
                if let Some(d) = ev.delta
                    && !d.is_empty()
                {
                    out.push(Ok(LanguageModelCompletionEvent::Text(d)));
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(d) = ev.delta
                    && !d.is_empty()
                {
                    out.push(Ok(LanguageModelCompletionEvent::Thinking {
                        text: d,
                        signature: None,
                    }));
                }
            }
            "response.output_item.added" => {
                if let (Some(idx), Some(item)) = (ev.output_index, ev.item.as_ref())
                    && is_function_call_item(item)
                {
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.tool_uses_by_index.insert(
                        idx,
                        RawToolUse {
                            id,
                            name: Arc::from(name),
                            input_json: String::new(),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                if let (Some(idx), Some(delta)) = (ev.output_index, ev.delta.as_ref())
                    && let Some(slot) = self.tool_uses_by_index.get_mut(&idx)
                {
                    slot.input_json.push_str(delta);
                    if let Some(event) = Self::try_emit_partial(slot) {
                        out.push(Ok(event));
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(idx) = ev.output_index
                    && let Some(mut slot) = self.tool_uses_by_index.remove(&idx)
                {
                    if let Some(final_args) = ev.arguments {
                        slot.input_json = final_args;
                    }
                    if let Some(event) = self.emit_complete(&mut slot) {
                        out.push(Ok(event));
                    }
                }
            }
            "response.output_item.done" => {
                if let (Some(idx), Some(item)) = (ev.output_index, ev.item.as_ref())
                    && is_function_call_item(item)
                {
                    // Use the canonical id/name/arguments from the done event.
                    // The slot is still in the map only when the matching
                    // `function_call_arguments.done` was missed — in that case
                    // the `arguments` here are the authoritative final value.
                    let canonical_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(String::from);
                    let canonical_name = item.get("name").and_then(Value::as_str).map(Arc::from);
                    if let Some(mut slot) = self.tool_uses_by_index.remove(&idx) {
                        if let Some(cid) = canonical_id {
                            slot.id = cid;
                        }
                        if let Some(cname) = canonical_name {
                            slot.name = cname;
                        }
                        if let Some(args) = item.get("arguments") {
                            slot.input_json = match args {
                                Value::String(s) => s.clone(),
                                _ => args.to_string(),
                            };
                        }
                        if let Some(event) = self.emit_complete(&mut slot) {
                            out.push(Ok(event));
                        }
                    }
                }
            }
            "response.completed" => {
                if let Some(resp) = ev.response.as_ref() {
                    if let Some(u) = resp.get("usage") {
                        update_responses_usage(&mut self.usage, u);
                    }
                    if let Some(reason) = resp.get("status").and_then(Value::as_str) {
                        self.stop_reason = match reason {
                            "completed" => StopReason::EndTurn,
                            "incomplete" => StopReason::MaxTokens,
                            "failed" => StopReason::Refusal,
                            _ => StopReason::EndTurn,
                        };
                    }
                }
                if self.usage.input_tokens > 0 || self.usage.output_tokens > 0 {
                    out.push(Ok(LanguageModelCompletionEvent::UsageUpdate(self.usage)));
                }
                out.push(Ok(LanguageModelCompletionEvent::Stop(self.stop_reason)));
                self.stop_emitted = true;
            }
            "response.incomplete" => {
                if let Some(resp) = ev.response.as_ref()
                    && let Some(u) = resp.get("usage")
                {
                    update_responses_usage(&mut self.usage, u);
                }
                if self.usage.input_tokens > 0 || self.usage.output_tokens > 0 {
                    out.push(Ok(LanguageModelCompletionEvent::UsageUpdate(self.usage)));
                }
                out.push(Ok(LanguageModelCompletionEvent::Stop(
                    StopReason::MaxTokens,
                )));
                self.stop_emitted = true;
            }
            "response.failed" | "error" => {
                let msg = ev
                    .response
                    .as_ref()
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| "Responses API returned an error".to_string());
                out.push(Err(anyhow!(msg)));
            }
            _ => {}
        }
        out
    }

    /// Emit a `ToolUse { is_input_complete: false }` if the current partial buffer parses cleanly.
    fn try_emit_partial(slot: &RawToolUse) -> Option<LanguageModelCompletionEvent> {
        if slot.id.is_empty() {
            return None;
        }
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

    /// Emit the final `ToolUse { is_input_complete: true }` and mark the call as finished.
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

    /// Drain any still-open tool slots at end of stream (e.g. provider closed the
    /// stream without sending `function_call_arguments.done`).
    fn flush_pending(&mut self) -> Vec<LanguageModelCompletionEvent> {
        let mut out = Vec::new();
        let indices: Vec<usize> = self.tool_uses_by_index.keys().copied().collect();
        for idx in indices {
            if let Some(mut slot) = self.tool_uses_by_index.remove(&idx)
                && let Some(ev) = self.emit_complete(&mut slot)
            {
                out.push(ev);
            }
        }
        out
    }
}

fn is_function_call_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call")
}

fn update_responses_usage(usage: &mut TokenUsage, value: &Value) {
    if let Some(v) = value.get("input_tokens").and_then(Value::as_u64) {
        usage.input_tokens = v;
    }
    if let Some(v) = value.get("output_tokens").and_then(Value::as_u64) {
        usage.output_tokens = v;
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
    use std::sync::Arc;

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
        assert_eq!(tools[0]["name"], "bash");
        assert!(tools[0]["description"].is_string());
        assert!(tools[0]["parameters"].is_object());
    }

    #[test]
    fn build_request_body_includes_reasoning_effort() {
        let mut req = req_with_tool();
        req.reasoning_effort = Some(ReasoningEffort::XHigh);
        let body = build_request_body("m", 64, &req, "test-key", false);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn build_request_body_clamps_reasoning_effort_for_official_openai() {
        let mut req = req_with_tool();
        req.reasoning_effort = Some(ReasoningEffort::Max);
        let body = build_request_body("m", 64, &req, "test-key", true);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn build_request_body_includes_ultracode_effort() {
        let mut req = req_with_tool();
        req.reasoning_effort = Some(ReasoningEffort::Ultracode);
        let body = build_request_body("m", 64, &req, "test-key", false);
        assert_eq!(body["reasoning"]["effort"], "ultracode");
    }

    #[test]
    fn build_input_emits_function_call_and_output() {
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
        let (input, instructions) = build_input(&messages);
        assert!(instructions.is_empty());
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["name"], "bash");
        assert_eq!(input[0]["arguments"], r#"{"cmd":"ls"}"#);
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["output"], "file.rs");
    }

    #[test]
    fn build_input_keeps_text_message_separate() {
        let messages = vec![LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text("hello".to_string())],
            cache: false,
        }];
        let (input, instructions) = build_input(&messages);
        assert!(instructions.is_empty());
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn build_input_lifts_system_to_instructions() {
        let messages = vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text("you are a sub-agent".to_string())],
                cache: false,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text("hi".to_string())],
                cache: false,
            },
        ];
        let (input, instructions) = build_input(&messages);
        // System text is lifted out of the input items entirely.
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(instructions, "you are a sub-agent");
    }

    fn make_added(index: usize, call_id: &str, name: &str) -> Value {
        json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": {
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": "",
            }
        })
    }

    fn make_delta(index: usize, delta: &str) -> Value {
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": index,
            "delta": delta,
        })
    }

    fn make_done(index: usize, args: &str) -> Value {
        json!({
            "type": "response.function_call_arguments.done",
            "output_index": index,
            "arguments": args,
        })
    }

    #[test]
    fn map_event_assembles_streamed_tool_input() {
        let mut m = ResponsesEventMapper::new();
        let events: Vec<Value> = vec![
            make_added(0, "call_1", "bash"),
            make_delta(0, r#"{"cmd":"#),
            make_delta(0, r#""ls"}"#),
            make_done(0, r#"{"cmd":"ls"}"#),
            json!({"type": "response.completed", "response": {"status": "completed"}}),
        ];
        let mut all = Vec::new();
        for ev in &events {
            all.extend(m.map_event(ev));
        }
        let tools: Vec<_> = all
            .iter()
            .filter_map(|r| match r {
                Ok(LanguageModelCompletionEvent::ToolUse(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        // Expect at least one partial (is_input_complete=false) and exactly one final.
        assert!(tools.iter().any(|t| !t.is_input_complete));
        let finals: Vec<_> = tools.iter().filter(|t| t.is_input_complete).collect();
        assert_eq!(finals.len(), 1, "exactly one complete tool_use");
        assert_eq!(finals[0].id, "call_1");
        assert_eq!(&*finals[0].name, "bash");
        assert_eq!(finals[0].input["cmd"], "ls");
        // Stop emitted.
        assert!(m.stop_emitted());
    }

    #[test]
    fn map_event_handles_missing_done_via_output_item_done() {
        let mut m = ResponsesEventMapper::new();
        let events: Vec<Value> = vec![
            make_added(0, "call_x", "read_file"),
            make_delta(0, r#"{"path":"a"#),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_x",
                    "name": "read_file",
                    "arguments": r#"{"path":"a.rs"}"#
                }
            }),
        ];
        let mut all = Vec::new();
        for ev in &events {
            all.extend(m.map_event(ev));
        }
        let finals: Vec<_> = all
            .iter()
            .filter_map(|r| match r {
                Ok(LanguageModelCompletionEvent::ToolUse(t)) if t.is_input_complete => {
                    Some(t.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].input["path"], "a.rs");
    }

    /// Live streaming test: send "hi" via the Bailian qwen3.7-plus responses wire.
    #[tokio::test]
    async fn live_responses_stream() {
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
                    && m.wire_api == WireApi::Responses
            })
            .expect("应含百炼 qwen3.7-plus responses");
        let api_key = crate::provider::resolve_apikey(
            model
                .apikey_source
                .as_deref()
                .unwrap_or("env:DASHSCOPE_API_KEY"),
        )
        .expect("resolve api key");

        let (tx, rx) = async_channel::bounded(64);
        let tx_clone = tx.clone();
        let url = responses_url(&model.endpoint_url);
        let api_model = model.api_model_id();
        let mut request = LanguageModelRequest::default();
        request.messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text("hi".to_string())],
            cache: false,
        });
        tokio::spawn(async move {
            if let Err(e) = stream_responses(
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
}
