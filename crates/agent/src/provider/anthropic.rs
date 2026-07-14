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
    /// Auto-compact window override from the provider config's
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` env var; `None` = no override.
    auto_compact_window: Option<u64>,
    /// Whether the endpoint is real Anthropic (api.anthropic.com) and thus
    /// eligible for the long (1h) cache TTL + the extended-cache-ttl beta header.
    long_ttl: bool,
    /// Whether the model accepts `output_config.effort` (Opus 4.5+ / Sonnet
    /// 4.6+ / Fable 5 / Mythos 5). Older Claude and non-Claude models on an
    /// Anthropic-compatible wire do not — gating avoids a 400 and keeps the
    /// effort selector a no-op for them (same as today).
    supports_effort: bool,
    /// cx agent ids this model can drive (from the endpoint `agents:` list).
    visible_agents: Vec<String>,
    /// Operator-declared tool capability (provider config `supports_tools`).
    supports_tools: bool,
    /// Operator-declared image capability (provider config `supports_images`).
    supports_images: bool,
}

/// Construction inputs for [`AnthropicModel`]. Bundled into a struct so the
/// builder stays under clippy's `too_many_arguments` threshold without an
/// `#[allow]`, and so call sites name fields instead of counting positions.
pub struct AnthropicModelConfig {
    pub id: String,
    pub name: String,
    pub provider_name: String,
    pub api_model_id: String,
    pub endpoint_url: String,
    pub api_key: String,
    pub max_token_count: u64,
    /// Resolved single-response output budget (operator-declared, capped at
    /// `max_token_count`; falls back to the heuristic clamp in `build_model`).
    pub max_output_tokens: u64,
    pub supports_tools: bool,
    pub supports_images: bool,
    pub auto_compact_window: Option<u64>,
    pub visible_agents: Vec<String>,
}

impl AnthropicModel {
    /// Build from a `ResolvedModel` (the api_key is already resolved at this point).
    pub fn new(cfg: AnthropicModelConfig) -> Self {
        let AnthropicModelConfig {
            id,
            name,
            provider_name,
            api_model_id,
            endpoint_url,
            api_key,
            max_token_count,
            max_output_tokens,
            supports_tools,
            supports_images,
            auto_compact_window,
            visible_agents,
        } = cfg;
        let long_ttl = crate::provider::anthropic_cache::supports_long_ttl(
            crate::provider::anthropic_cache::resolve_prompt_caching_policy(
                None,
                Some(&endpoint_url),
            ),
            Some(&endpoint_url),
        );
        let supports_effort = crate::provider::anthropic_supports_effort(&api_model_id);
        Self {
            id,
            name,
            provider_name,
            api_model_id,
            endpoint_url: endpoint_url.clone(),
            api_key,
            max_output_tokens,
            max_token_count,
            auto_compact_window,
            long_ttl,
            supports_effort,
            visible_agents,
            supports_tools,
            supports_images,
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
    fn visible_agents(&self) -> &[String] {
        &self.visible_agents
    }
    fn supports_tools(&self) -> bool {
        self.supports_tools
    }
    fn supports_images(&self) -> bool {
        self.supports_images
    }
    fn max_token_count(&self) -> u64 {
        self.max_token_count
    }

    fn auto_compact_window(&self) -> Option<u64> {
        self.auto_compact_window
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
        let supports_effort = self.supports_effort;

        Box::pin(async move {
            let (tx, rx) = async_channel::bounded::<Result<LanguageModelCompletionEvent>>(64);
            let tx_clone = tx.clone();
            crate::runtime::handle().spawn(async move {
                if let Err(e) = stream_anthropic(
                    &url,
                    &api_key,
                    &model,
                    max_tokens,
                    request,
                    tx_clone,
                    policy,
                    long_ttl,
                    supports_effort,
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
    supports_effort: bool,
) -> Result<()> {
    let body = build_request_body(
        model,
        max_tokens,
        &request,
        policy,
        long_ttl,
        supports_effort,
    )?;
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
    supports_effort: bool,
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
    // `output_config.effort` is a top-level field (not part of the cached
    // message prefix, so toggling it does not break prefix caching). The value
    // is already resolved to a concrete level by `build_completion_request` —
    // `Auto` / `Ultracode` never reach here. Gated on `supports_effort` so
    // older Claude and non-Claude Anthropic-wire models do not 400.
    if supports_effort && let Some(effort) = request.reasoning_effort {
        body["output_config"] = json!({ "effort": effort.wire_value() });
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
        // Compaction is rewritten to Text by `model_facing_content` before the
        // request reaches the provider; reached here only if that transform was
        // skipped, so emit the summary as plain text rather than dropping it.
        MessageContent::Compaction(t) => Some(json!({"type": "text", "text": t})),
        MessageContent::Thinking { text, signature } => {
            // Anthropic requires a signature on every thinking block it receives
            // as input; a signature-less block (e.g. sourced from a completions
            // wire, where `reasoning_content` carries no signature) would 400.
            // Drop it rather than emit an invalid block. Anthropic-native
            // thinking always carries a signature (SignatureDelta), so this
            // only filters thinking imported from a non-Anthropic wire.
            let sig = signature.as_deref().filter(|s| !s.is_empty())?;
            Some(json!({
                "type": "thinking",
                "thinking": text,
                "signature": sig,
            }))
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
                    // Surface the raw usage the upstream returned so a provider
                    // that never populates Anthropic prompt-caching stats (most
                    // non-Claude Anthropic-compatible gateways) is visible: the
                    // "消费" card's 缓存 branch stays 0 not because manox dropped
                    // the field, but because the gateway never sent it.
                    tracing::debug!(
                        input = ?u.input_tokens,
                        output = ?u.output_tokens,
                        cache_creation = ?u.cache_creation_input_tokens,
                        cache_read = ?u.cache_read_input_tokens,
                        "anthropic MessageStart usage",
                    );
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
    use crate::language_model::{LanguageModelRequestMessage, MessageContent, ReasoningEffort};
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

    /// `AnthropicModel::new` stores the resolved `max_output_tokens` from the
    /// config verbatim — it does not re-derive or re-clamp. Output-budget
    /// resolution (operator-declared value capped at the context window, else
    /// the heuristic clamp) happens in `registry::resolve_max_output_tokens`
    /// before the config is built; `::new` is a plain store so the resolved
    /// value reaches the request body unchanged.
    #[test]
    fn new_stores_max_output_tokens_from_config_verbatim() {
        let big = AnthropicModel::new(AnthropicModelConfig {
            id: "p/m/anthropic".into(),
            name: "m[1m]".into(),
            provider_name: "p".into(),
            api_model_id: "m".into(),
            endpoint_url: "https://example.invalid".into(),
            api_key: "k".into(),
            max_token_count: 1024 * 1024,
            max_output_tokens: 65_536,
            supports_tools: true,
            supports_images: false,
            auto_compact_window: None,
            visible_agents: Vec::new(),
        });
        assert_eq!(
            big.max_output_tokens, 65_536,
            "stored verbatim, not re-clamped"
        );
        let tiny = AnthropicModel::new(AnthropicModelConfig {
            id: "p/m/anthropic".into(),
            name: "m".into(),
            provider_name: "p".into(),
            api_model_id: "m".into(),
            endpoint_url: "https://example.invalid".into(),
            api_key: "k".into(),
            max_token_count: 4_096,
            max_output_tokens: 1_024,
            supports_tools: true,
            supports_images: false,
            auto_compact_window: None,
            visible_agents: Vec::new(),
        });
        assert_eq!(tiny.max_output_tokens, 1_024);
    }

    /// `supports_tools` / `supports_images` round-trip from the config struct
    /// into the stored fields and the `LanguageModel` trait overrides.
    #[test]
    fn new_stores_capability_flags_from_config() {
        let caps = AnthropicModel::new(AnthropicModelConfig {
            id: "p/m/anthropic".into(),
            name: "m".into(),
            provider_name: "p".into(),
            api_model_id: "m".into(),
            endpoint_url: "https://example.invalid".into(),
            api_key: "k".into(),
            max_token_count: 8_192,
            max_output_tokens: 8_192,
            supports_tools: false,
            supports_images: true,
            auto_compact_window: None,
            visible_agents: Vec::new(),
        });
        assert!(!caps.supports_tools());
        assert!(caps.supports_images());
    }

    /// `auto_compact_window` round-trips from the config struct into the stored
    /// field unchanged — a regression guard for the `AnthropicModelConfig`
    /// bundling (destructure must not drop or swap the field).
    #[test]
    fn new_stores_auto_compact_window_from_config() {
        let with_override = AnthropicModel::new(AnthropicModelConfig {
            id: "p/m/anthropic".into(),
            name: "m".into(),
            provider_name: "p".into(),
            api_model_id: "m".into(),
            endpoint_url: "https://example.invalid".into(),
            api_key: "k".into(),
            max_token_count: 8_192,
            auto_compact_window: Some(202_745),
            visible_agents: Vec::new(),
            max_output_tokens: 8_192,
            supports_tools: true,
            supports_images: false,
        });
        assert_eq!(with_override.auto_compact_window, Some(202_745));

        let without_override = AnthropicModel::new(AnthropicModelConfig {
            id: "p/m/anthropic".into(),
            name: "m".into(),
            provider_name: "p".into(),
            api_model_id: "m".into(),
            endpoint_url: "https://example.invalid".into(),
            api_key: "k".into(),
            max_token_count: 8_192,
            auto_compact_window: None,
            visible_agents: Vec::new(),
            max_output_tokens: 8_192,
            supports_tools: true,
            supports_images: false,
        });
        assert_eq!(without_override.auto_compact_window, None);
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
            let supports_effort = crate::provider::anthropic_supports_effort(&api_model);
            if let Err(e) = stream_anthropic(
                &url,
                &api_key,
                &api_model,
                512,
                simple_request("hi"),
                tx_clone,
                policy,
                long_ttl,
                supports_effort,
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

    fn request_with_effort(effort: ReasoningEffort) -> LanguageModelRequest {
        let mut req = simple_request("hi");
        req.reasoning_effort = Some(effort);
        req
    }

    #[test]
    fn build_request_body_emits_output_config_effort_when_supported() {
        let req = request_with_effort(ReasoningEffort::High);
        let body = build_request_body(
            "claude-opus-4-8",
            64,
            &req,
            crate::provider::anthropic_cache::PromptCachingPolicy::None,
            false,
            true,
        )
        .unwrap();
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn build_request_body_omits_output_config_when_unsupported_model() {
        let req = request_with_effort(ReasoningEffort::High);
        let body = build_request_body(
            "claude-3-7-sonnet",
            64,
            &req,
            crate::provider::anthropic_cache::PromptCachingPolicy::None,
            false,
            false,
        )
        .unwrap();
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn build_request_body_omits_output_config_when_effort_none() {
        let req = simple_request("hi");
        let body = build_request_body(
            "claude-opus-4-8",
            64,
            &req,
            crate::provider::anthropic_cache::PromptCachingPolicy::None,
            false,
            true,
        )
        .unwrap();
        assert!(body.get("output_config").is_none());
    }

    // --- Live cache semantics probes (Bailian anthropic wire) ---
    // Require MANOX_RUN_LIVE=1 and the DASHSCOPE_API_KEY in the macOS Keychain
    // (per the configured glm-5.2[1m] anthropic model). They answer three
    // questions about how Bailian's anthropic-compatible endpoint treats
    // explicit `cache_control` breakpoints:
    //   1. Does a breakpoint on the last *tool* (manox's current
    //      LastBreakpointOnly policy output) produce a cache_read on the second
    //      identical-prefix request?
    //   2. Does a breakpoint on the *system* block (Bailian's documented
    //      pattern) produce a cache_read?
    //   3. Does Bailian honor *multiple* breakpoints, or only the last one?

    async fn bailian_anthropic_endpoint() -> Option<(String, String, String)> {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return None;
        }
        let config = crate::provider::CxConfig::load_default().ok()?;
        let model = config.resolve_all_models().into_iter().find(|m| {
            m.provider_name == "百炼"
                && m.id.contains("glm-5.2")
                && m.wire_api == WireApi::Anthropic
        })?;
        let api_key = crate::provider::resolve_apikey(
            model
                .apikey_source
                .as_deref()
                .unwrap_or("env:DASHSCOPE_API_KEY"),
        )
        .ok()?;
        let url = messages_url(&model.endpoint_url);
        Some((url, api_key, model.api_model_id()))
    }

    /// A prefix well above Bailian's 1024-token minimum cacheable block.
    fn cacheable_prefix() -> String {
        "The quick brown fox jumps over the lazy dog. ".repeat(150)
    }

    /// Send a hand-built Anthropic Messages body and collect the max
    /// `input_tokens` / `cache_creation_input_tokens` / `cache_read_input_tokens`
    /// reported across SSE usage events. Bypasses `apply_prompt_caching` so the
    /// caller controls breakpoint placement exactly.
    async fn probe_cache(
        url: &str,
        api_key: &str,
        model: &str,
        mut body: serde_json::Value,
    ) -> (u64, u64, u64) {
        use futures::StreamExt;
        body["model"] = serde_json::json!(model);
        body["max_tokens"] = serde_json::json!(64);
        body["stream"] = serde_json::json!(true);
        let client = reqwest::Client::builder().build().expect("client");
        let resp = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await
            .expect("send");
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            panic!("HTTP {status}: {text}");
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut input = 0u64;
        let mut creation = 0u64;
        let mut read = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("chunk");
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_string();
                buf = buf[nl + 1..].to_string();
                let Some(data) = extract_data_line(&line) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                if let Some(u) = v.get("usage") {
                    if let Some(n) = u.get("input_tokens").and_then(|n| n.as_u64()) {
                        input = input.max(n);
                    }
                    if let Some(n) = u
                        .get("cache_creation_input_tokens")
                        .and_then(|n| n.as_u64())
                    {
                        creation = creation.max(n);
                    }
                    if let Some(n) = u.get("cache_read_input_tokens").and_then(|n| n.as_u64()) {
                        read = read.max(n);
                    }
                }
            }
        }
        eprintln!("[PROBE] input={input} creation={creation} read={read}");
        (input, creation, read)
    }

    /// Probe 1: manox's current LastBreakpointOnly policy places the breakpoint
    /// on the last *tool* and leaves `system` a bare string (no cache_control).
    /// Does Bailian honor a tool-block breakpoint — i.e. does the second
    /// identical-prefix request show cache_read > 0?
    #[tokio::test]
    async fn live_anthropic_cache_last_tool_breakpoint() {
        let Some((url, key, model)) = bailian_anthropic_endpoint().await else {
            return;
        };
        let prefix = cacheable_prefix();
        let body_for = |user: &str| {
            serde_json::json!({
                "system": prefix,
                "tools": [{
                    "name": "dummy",
                    "description": "placeholder tool to host a cache_control breakpoint",
                    "input_schema": {"type":"object","properties":{}},
                    "cache_control": {"type":"ephemeral"}
                }],
                "messages": [{"role":"user","content":user}],
            })
        };
        let (_, _, read1) =
            probe_cache(&url, &key, &model, body_for("what is the code about?")).await;
        let (_, _, read2) =
            probe_cache(&url, &key, &model, body_for("how to optimize the code?")).await;
        eprintln!(
            "[PROBE1 last-tool] read1={read1} read2={read2} (read2>0 → Bailian honors tool-block breakpoint)"
        );
    }

    /// Probe 2: Bailian's documented pattern — breakpoint on the *system* block,
    /// no tools. Does the second request show cache_read > 0?
    #[tokio::test]
    async fn live_anthropic_cache_system_breakpoint() {
        let Some((url, key, model)) = bailian_anthropic_endpoint().await else {
            return;
        };
        let prefix = cacheable_prefix();
        let body_for = |user: &str| {
            serde_json::json!({
                "system": [{"type":"text","text":prefix,"cache_control":{"type":"ephemeral"}}],
                "messages": [{"role":"user","content":user}],
            })
        };
        let (_, _, read1) =
            probe_cache(&url, &key, &model, body_for("what is the code about?")).await;
        let (_, _, read2) =
            probe_cache(&url, &key, &model, body_for("how to optimize the code?")).await;
        eprintln!(
            "[PROBE2 system] read1={read1} read2={read2} (read2>0 → Bailian honors system-block breakpoint)"
        );
    }

    /// Probe 3: two breakpoints (system block + a prefix user message). Does
    /// Bailian honor both, or only the last one? system alone is ~1500 tokens;
    /// the shared context-prefix is ~10 tokens — so read2 magnitude tells both
    // (~1510) vs last-only (~10).
    #[tokio::test]
    async fn live_anthropic_cache_multi_breakpoint() {
        let Some((url, key, model)) = bailian_anthropic_endpoint().await else {
            return;
        };
        let prefix = cacheable_prefix();
        let body_for = |user: &str| {
            serde_json::json!({
                "system": [{"type":"text","text":prefix,"cache_control":{"type":"ephemeral"}}],
                "messages": [
                    {"role":"user","content":[{"type":"text","text":"shared context prefix","cache_control":{"type":"ephemeral"}}]},
                    {"role":"assistant","content":"ack"},
                    {"role":"user","content":user}
                ],
            })
        };
        let (_, _, read1) =
            probe_cache(&url, &key, &model, body_for("what is the code about?")).await;
        let (_, _, read2) =
            probe_cache(&url, &key, &model, body_for("how to optimize the code?")).await;
        eprintln!(
            "[PROBE3 multi] read1={read1} read2={read2} (read2≈1500 → both honored; read2≈10 → last-only)"
        );
    }

    /// Anthropic requires a signature on every thinking block received as
    /// input. A signature-less block (imported from a completions wire, where
    /// `reasoning_content` carries no signature) must be dropped rather than
    /// emitted as an invalid `{"type":"thinking", ...}` without a signature —
    /// otherwise a thread with mixed-wire history 400s when sent to Anthropic.
    #[test]
    fn drops_signature_less_thinking_and_keeps_signed() {
        let dropped = content_to_anthropic(&MessageContent::Thinking {
            text: "imported from completions".into(),
            signature: None,
        });
        assert!(dropped.is_none(), "signature-less thinking must be dropped");

        let empty_sig = content_to_anthropic(&MessageContent::Thinking {
            text: "empty signature".into(),
            signature: Some(String::new()),
        });
        assert!(
            empty_sig.is_none(),
            "empty-string signature must be dropped"
        );

        let kept = content_to_anthropic(&MessageContent::Thinking {
            text: "native anthropic thinking".into(),
            signature: Some("sig-0".into()),
        })
        .expect("signed thinking is emitted");
        assert_eq!(kept["type"], "thinking");
        assert_eq!(kept["thinking"], "native anthropic thinking");
        assert_eq!(kept["signature"], "sig-0");
    }
}
