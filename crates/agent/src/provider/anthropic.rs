//! `LanguageModel` implementation for the Anthropic wire.
//!
//! - Request: POST `{endpoint}/v1/messages`, `x-api-key` (or Bearer) + `anthropic-version`.
//! - Response: SSE `data:` lines, mapped to `LanguageModelCompletionEvent` by `AnthropicEventMapper`.
//! - Streaming: a tokio task runs reqwest + SSE parsing and forwards events back to the gpui-side `BoxStream` via `async_channel`.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;
use serde::Deserialize;

use crate::language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    LanguageModelToolUse, MessageContent, Role, StopReason, TokenUsage,
};
use crate::provider::sse::{extract_data_line, fix_streamed_json};

/// Language model over the Anthropic wire.
pub struct AnthropicModel {
    /// Stable internal manox id (`provider/model/wire`).
    id: String,
    /// Display name (including the `[1m]` suffix).
    name: String,
    provider_name: String,
    /// model id sent to the API (context suffix like `[1m]` stripped).
    api_model_id: String,
    endpoint_url: String,
    api_key: String,
    max_output_tokens: u64,
    max_token_count: u64,
    /// Whether the endpoint is real Anthropic (api.anthropic.com) and thus
    /// eligible for the long (1h) cache TTL + the extended-cache-ttl beta header.
    long_ttl: bool,
}

impl AnthropicModel {
    /// Build from a `ResolvedModel` (the api_key is already resolved at this point).
    pub fn new(
        id: String,
        name: String,
        provider_name: String,
        api_model_id: String,
        endpoint_url: String,
        api_key: String,
        max_token_count: u64,
    ) -> Self {
        let long_ttl = crate::provider::anthropic_cache::supports_long_ttl(
            crate::provider::anthropic_cache::resolve_prompt_caching_policy(
                None,
                Some(&endpoint_url),
            ),
            Some(&endpoint_url),
        );
        Self {
            id,
            name,
            provider_name,
            api_model_id,
            endpoint_url: endpoint_url.clone(),
            api_key,
            max_output_tokens: max_token_count.min(8192),
            max_token_count,
            long_ttl,
        }
    }
}

impl LanguageModel for AnthropicModel {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn provider_id(&self) -> String {
        format!("anthropic:{}", self.provider_name)
    }
    fn provider_name(&self) -> String {
        self.provider_name.clone()
    }
    fn wire_api(&self) -> crate::provider::WireApi {
        crate::provider::WireApi::Anthropic
    }
    fn max_token_count(&self) -> u64 {
        self.max_token_count
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        _cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<LanguageModelCompletionEvent>>>> {
        let url = messages_url(&self.endpoint_url);
        let api_key = self.api_key.clone();
        let model = self.api_model_id.clone();
        let max_tokens = self.max_output_tokens;
        let policy = crate::provider::anthropic_cache::resolve_prompt_caching_policy(
            None,
            Some(&self.endpoint_url),
        );
        let long_ttl = self.long_ttl;

        Box::pin(async move {
            let (tx, rx) = async_channel::bounded::<Result<LanguageModelCompletionEvent>>(64);
            let tx_clone = tx.clone();
            crate::runtime::handle().spawn(async move {
                if let Err(e) = stream_anthropic(
                    &url, &api_key, &model, max_tokens, request, tx_clone, policy, long_ttl,
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

/// Build the messages-endpoint URL (tolerates an endpoint that already contains a full path).
fn messages_url(endpoint: &str) -> String {
    if endpoint.ends_with("/v1/messages") {
        endpoint.to_string()
    } else {
        format!("{}/v1/messages", endpoint.trim_end_matches('/'))
    }
}

/// Send the request, parse the SSE stream, map events, and forward them through `tx`.
#[allow(clippy::too_many_arguments)]
pub async fn stream_anthropic(
    url: &str,
    api_key: &str,
    model: &str,
    max_tokens: u64,
    request: LanguageModelRequest,
    tx: async_channel::Sender<Result<LanguageModelCompletionEvent>>,
    policy: crate::provider::anthropic_cache::PromptCachingPolicy,
    long_ttl: bool,
) -> Result<()> {
    let body = build_request_body(model, max_tokens, &request, policy, long_ttl)?;
    let client = reqwest::Client::builder()
        .build()
        .context("Failed to build reqwest client")?;

    // Real Anthropic + long TTL needs the extended-cache-ttl beta header to
    // activate the 1h breakpoint lifetime.
    let beta_header = if long_ttl {
        Some(crate::provider::anthropic_cache::EXTENDED_CACHE_TTL_BETA)
    } else {
        None
    };
    // Retry the handshake (429 / 5xx / network errors) before any SSE event is
    // forwarded. The body is captured by reference so every attempt sends the
    // byte-identical request — provider-side prefix caching is unaffected.
    let api_key_owned = api_key.to_string();
    let url_owned = url.to_string();
    let response = match crate::provider::retry::send_with_retry(
        || {
            let client = client.clone();
            let mut req = client
                .post(&url_owned)
                .header("Content-Type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .header("x-api-key", &api_key_owned);
            if let Some(beta) = beta_header {
                req = req.header("anthropic-beta", beta);
            }
            req.json(&body).send()
        },
        &tx,
        "Anthropic API",
    )
    .await
    {
        Ok(resp) => resp,
        // Terminal failure or cancellation — the error has already been
        // forwarded through `tx` (or the receiver was dropped). Stop the stream
        // without emitting a second error.
        Err(_) => return Ok(()),
    };

    let mut mapper = AnthropicEventMapper::new();
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
            let event: AnthropicEvent = match serde_json::from_str(data) {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx
                        .send(Err(anyhow!("Failed to parse SSE event: {e}")))
                        .await;
                    continue;
                }
            };
            for mapped in mapper.map_event(event) {
                if tx.send(mapped).await.is_err() {
                    return Ok(()); // Receiver dropped; stop.
                }
            }
        }
    }

    // A stream that ends without MessageStop is incomplete — the model may have
    // been producing text the consumer never sees as terminal. Surface it as an
    // error event so the caller can persist whatever was received rather than
    // silently dropping the turn.
    if !mapper.saw_message_stop {
        let _ = tx
            .send(Err(anyhow!(
                "Anthropic stream ended without a MessageStop event"
            )))
            .await;
    }
    Ok(())
}

/// Build the Anthropic messages request body.
fn build_request_body(
    model: &str,
    max_tokens: u64,
    request: &LanguageModelRequest,
    policy: crate::provider::anthropic_cache::PromptCachingPolicy,
    long_ttl: bool,
) -> Result<serde_json::Value> {
    use serde_json::{Value, json};

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for msg in &request.messages {
        if msg.role == Role::System {
            if let Some(s) = string_of_message(msg) {
                system_parts.push(s);
            }
            continue;
        }
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };
        let content: Vec<Value> = msg
            .content
            .iter()
            .filter_map(content_to_anthropic)
            .collect();
        if content.is_empty() {
            continue;
        }
        messages.push(json!({"role": role, "content": content}));
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });
    if !system_parts.is_empty() {
        body["system"] = Value::String(system_parts.join("\n\n"));
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect(),
        );
    }

    // Place cache_control breakpoints according to the resolved policy. Done
    // after body construction so it can upgrade the system string to blocks
    // and mark the last tool / last message text blocks uniformly.
    crate::provider::anthropic_cache::apply_prompt_caching(&mut body, policy, long_ttl);

    Ok(body)
}

fn string_of_message(msg: &LanguageModelRequestMessage) -> Option<String> {
    let s = msg.string_contents();
    if s.is_empty() { None } else { Some(s) }
}

/// Convert a `MessageContent` into an Anthropic content-block JSON value.
fn content_to_anthropic(c: &MessageContent) -> Option<serde_json::Value> {
    use serde_json::json;
    match c {
        MessageContent::Text(t) => Some(json!({"type": "text", "text": t})),
        MessageContent::Thinking { text, signature } => {
            let mut v = json!({"type": "thinking", "thinking": text});
            if let Some(sig) = signature {
                v["signature"] = serde_json::Value::String(sig.clone());
            }
            Some(v)
        }
        MessageContent::ToolUse(tu) => Some(json!({
            "type": "tool_use",
            "id": tu.id,
            "name": tu.name,
            "input": tu.input,
        })),
        MessageContent::ToolResult(tr) => {
            let mut v = json!({
                "type": "tool_result",
                "tool_use_id": tr.tool_use_id,
                "content": tr.content,
            });
            if tr.is_error {
                v["is_error"] = serde_json::Value::Bool(true);
            }
            Some(v)
        }
        MessageContent::Image { data, mime_type } => Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime_type, "data": data},
        })),
    }
}

// ─── SSE event types (Anthropic streaming API) ───────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicEvent {
    MessageStart {
        message: AnthropicMessage,
    },
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    Error {
        error: AnthropicErrorPayload,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnthropicMessage {
    id: String,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorPayload {
    message: String,
}

// ─── Anthropic SSE event mapper ──────────────────────────────────────────

struct AnthropicEventMapper {
    tool_uses_by_index: HashMap<usize, RawToolUse>,
    usage: TokenUsage,
    stop_reason: StopReason,
    /// Whether a `MessageStop` event has been seen. A stream that ends without
    /// one terminated abnormally (provider hiccup, non-SSE compatibility
    /// response) — `stream_anthropic` surfaces this as an error so the caller
    /// does not mistake a truncated stream for a clean turn end.
    saw_message_stop: bool,
}

struct RawToolUse {
    id: String,
    name: String,
    input_json: String,
}

impl AnthropicEventMapper {
    fn new() -> Self {
        Self {
            tool_uses_by_index: HashMap::new(),
            usage: TokenUsage::default(),
            stop_reason: StopReason::EndTurn,
            saw_message_stop: false,
        }
    }

    fn map_event(&mut self, event: AnthropicEvent) -> Vec<Result<LanguageModelCompletionEvent>> {
        match event {
            AnthropicEvent::MessageStart { message } => {
                if let Some(u) = message.usage {
                    update_usage(&mut self.usage, &u);
                }
                vec![Ok(LanguageModelCompletionEvent::UsageUpdate(self.usage))]
            }
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => match content_block {
                AnthropicContentBlock::Text { text } => {
                    vec![Ok(LanguageModelCompletionEvent::Text(text))]
                }
                AnthropicContentBlock::Thinking { thinking } => {
                    vec![Ok(LanguageModelCompletionEvent::Thinking {
                        text: thinking,
                        signature: None,
                    })]
                }
                AnthropicContentBlock::ToolUse { id, name } => {
                    self.tool_uses_by_index.insert(
                        index,
                        RawToolUse {
                            id,
                            name,
                            input_json: String::new(),
                        },
                    );
                    Vec::new()
                }
                AnthropicContentBlock::Other => Vec::new(),
            },
            AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                AnthropicDelta::TextDelta { text } => {
                    vec![Ok(LanguageModelCompletionEvent::Text(text))]
                }
                AnthropicDelta::ThinkingDelta { thinking } => {
                    vec![Ok(LanguageModelCompletionEvent::Thinking {
                        text: thinking,
                        signature: None,
                    })]
                }
                AnthropicDelta::SignatureDelta { signature } => {
                    vec![Ok(LanguageModelCompletionEvent::Thinking {
                        text: String::new(),
                        signature: Some(signature),
                    })]
                }
                AnthropicDelta::InputJsonDelta { partial_json } => {
                    if let Some(tool_use) = self.tool_uses_by_index.get_mut(&index) {
                        tool_use.input_json.push_str(&partial_json);
                        if let Ok(input) = fix_streamed_json(&tool_use.input_json) {
                            return vec![Ok(LanguageModelCompletionEvent::ToolUse(
                                LanguageModelToolUse {
                                    id: tool_use.id.clone(),
                                    name: std::sync::Arc::from(tool_use.name.clone()),
                                    is_input_complete: false,
                                    raw_input: tool_use.input_json.clone(),
                                    input,
                                    thought_signature: None,
                                },
                            ))];
                        }
                    }
                    Vec::new()
                }
                AnthropicDelta::Other => Vec::new(),
            },
            AnthropicEvent::ContentBlockStop { index } => {
                if let Some(tool_use) = self.tool_uses_by_index.remove(&index) {
                    let input_json = tool_use.input_json.trim();
                    let event = match serde_json::from_str::<serde_json::Value>(input_json) {
                        Ok(input) => Ok(LanguageModelCompletionEvent::ToolUse(
                            LanguageModelToolUse {
                                id: tool_use.id.clone(),
                                name: std::sync::Arc::from(tool_use.name.clone()),
                                is_input_complete: true,
                                input,
                                raw_input: tool_use.input_json.clone(),
                                thought_signature: None,
                            },
                        )),
                        Err(e) => Ok(LanguageModelCompletionEvent::ToolUseJsonParseError {
                            id: tool_use.id.clone(),
                            tool_name: std::sync::Arc::from(tool_use.name.clone()),
                            raw_input: input_json.to_string(),
                            json_parse_error: e.to_string(),
                        }),
                    };
                    vec![event]
                } else {
                    Vec::new()
                }
            }
            AnthropicEvent::MessageDelta { delta, usage } => {
                if let Some(u) = usage {
                    update_usage(&mut self.usage, &u);
                }
                if let Some(stop) = delta.stop_reason {
                    self.stop_reason = match stop.as_str() {
                        "end_turn" => StopReason::EndTurn,
                        "max_tokens" => StopReason::MaxTokens,
                        "tool_use" => StopReason::ToolUse,
                        "refusal" => StopReason::Refusal,
                        _ => StopReason::EndTurn,
                    };
                }
                vec![Ok(LanguageModelCompletionEvent::UsageUpdate(self.usage))]
            }
            AnthropicEvent::MessageStop => {
                self.saw_message_stop = true;
                vec![Ok(LanguageModelCompletionEvent::Stop(self.stop_reason))]
            }
            AnthropicEvent::Error { error } => {
                vec![Err(anyhow!("Anthropic stream error: {}", error.message))]
            }
            AnthropicEvent::Other => Vec::new(),
        }
    }
}

fn update_usage(usage: &mut TokenUsage, new: &AnthropicUsage) {
    if let Some(v) = new.input_tokens {
        usage.input_tokens = v;
    }
    if let Some(v) = new.output_tokens {
        usage.output_tokens = v;
    }
    if let Some(v) = new.cache_creation_input_tokens {
        usage.cache_creation_input_tokens = v;
    }
    if let Some(v) = new.cache_read_input_tokens {
        usage.cache_read_input_tokens = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{LanguageModelRequestMessage, MessageContent};
    use crate::provider::WireApi;

    fn simple_request(text: &str) -> LanguageModelRequest {
        LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(text.to_string())],
                cache: false,
            }],
            ..Default::default()
        }
    }

    /// Live streaming test: send "hi" via the Bailian glm-5.2[1m] anthropic wire.
    /// Requires `MANOX_RUN_LIVE=1` and DASHSCOPE_API_KEY in the macOS Keychain.
    #[tokio::test]
    async fn live_anthropic_stream() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return;
        }
        let config = crate::provider::CxConfig::load_default().expect("load config");
        let model = config
            .resolve_all_models()
            .into_iter()
            .find(|m| {
                m.provider_name == "百炼"
                    && m.id.contains("glm-5.2")
                    && m.wire_api == WireApi::Anthropic
            })
            .expect("应含百炼 glm-5.2[1m] anthropic");
        let api_key = crate::provider::resolve_apikey(
            model
                .apikey_source
                .as_deref()
                .unwrap_or("env:DASHSCOPE_API_KEY"),
        )
        .expect("resolve api key");

        let (tx, rx) = async_channel::bounded(64);
        let tx_clone = tx.clone();
        let url = messages_url(&model.endpoint_url);
        let api_model = model.api_model_id();
        tokio::spawn(async move {
            let policy = crate::provider::anthropic_cache::resolve_prompt_caching_policy(
                None,
                Some(&model.endpoint_url),
            );
            let long_ttl = crate::provider::anthropic_cache::supports_long_ttl(
                policy,
                Some(&model.endpoint_url),
            );
            if let Err(e) = stream_anthropic(
                &url,
                &api_key,
                &api_model,
                512,
                simple_request("hi"),
                tx_clone,
                policy,
                long_ttl,
            )
            .await
            {
                let _ = tx.send(Err(e)).await;
            }
        });

        let mut content_events = 0u32;
        let mut stopped = false;
        while let Ok(ev) = rx.recv().await {
            match ev {
                Ok(LanguageModelCompletionEvent::Text(_))
                | Ok(LanguageModelCompletionEvent::Thinking { .. }) => content_events += 1,
                Ok(LanguageModelCompletionEvent::Stop(_)) => {
                    stopped = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("stream error: {e}"),
            }
        }
        assert!(
            content_events > 0,
            "应至少收到一个内容事件（Text/Thinking）"
        );
        assert!(stopped, "应收到 Stop 事件");
    }

    /// `stream_anthropic` flags a stream that ends without `MessageStop` by
    /// emitting a trailing `Err` event. That contract rests on the mapper's
    /// `saw_message_stop` flag toggling exactly once on the `MessageStop` arm
    /// and never otherwise — pin it here so a future refactor can't silently
    /// break the only signal the turn-end backstop relies on.
    #[test]
    fn mapper_saw_message_stop_flag_contract() {
        let mut mapper = AnthropicEventMapper::new();

        // A text delta must NOT set the flag — the stream is still in flight.
        let events = mapper.map_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: AnthropicDelta::TextDelta {
                text: "partial".into(),
            },
        });
        assert!(events.iter().any(|e| matches!(
            e,
            Ok(LanguageModelCompletionEvent::Text(t)) if t == "partial"
        )));
        assert!(!mapper.saw_message_stop);

        // `MessageStop` is the only event that sets the flag.
        mapper.map_event(AnthropicEvent::MessageStop);
        assert!(mapper.saw_message_stop);
    }
}
