// Wire types for the Anthropic Messages API.
//
// These types mirror the API's request/response schema field-for-field. They
// exist only at the provider boundary: nothing outside `provider::anthropic`
// should name them. The domain types in `crate::types` are the cross-provider
// representation; translation to and from these wire types lives in
// `translate.rs`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ── Request ─────────────────────────────────────────────────────────────────

/// `POST /v1/messages` request body.
#[derive(Debug, Clone, Serialize)]
pub struct MessageCreateParams {
    pub model: String,
    pub max_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemBlock>>,
    pub messages: Vec<MessageParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolParam>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    pub stream: bool,
}

/// Controls the model's output. `effort` tunes how hard adaptive-thinking
/// models reason; it lives here, NOT inside `thinking`.
#[derive(Debug, Clone, Serialize)]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

/// How hard an adaptive-thinking model reasons.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// A system prompt block, optionally carrying a cache breakpoint.
#[derive(Debug, Clone, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlockParam>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Content blocks as sent to the API (request direction).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ContentBlockParam {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "image")]
    Image {
        source: ImageSourceParam,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlockParam>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

impl ContentBlockParam {
    /// Attach a cache breakpoint. Only the block kinds the API accepts a
    /// breakpoint on (text, image, tool_result) are marked; other kinds are
    /// left untouched.
    pub fn set_cache_control(&mut self, cache_control: CacheControl) {
        match self {
            ContentBlockParam::Text { cache_control: slot, .. }
            | ContentBlockParam::Image { cache_control: slot, .. }
            | ContentBlockParam::ToolResult { cache_control: slot, .. } => {
                *slot = Some(cache_control);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ImageSourceParam {
    #[serde(rename = "base64")]
    Base64 { media_type: String, data: String },
    #[serde(rename = "url")]
    Url { url: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolParam {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: JsonValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// The `thinking` request field. All three protocol variants are supported;
/// `budget_tokens` is intentionally omitted — reasoning depth is controlled by
/// `output_config.effort`, not by a token budget.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ThinkingConfig {
    /// Enable thinking without a token budget (the gateway/model decides how
    /// much to think; effort is set separately in `output_config`).
    #[serde(rename = "enabled")]
    Enabled {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// Explicitly disable thinking. Omitting the field entirely leaves the
    /// server default in place; this variant forces thinking off.
    #[serde(rename = "disabled")]
    Disabled,
    /// Adaptive thinking: the model decides when and how much to reason.
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "ephemeral"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>, // "5m" | "1h"
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        CacheControl { kind: "ephemeral", ttl: None }
    }
}

// ── SSE stream events ───────────────────────────────────────────────────────

/// One server-sent event from the streaming endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum RawStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: WireMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: usize, content_block: WireContentBlock },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: WireDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: MessageDeltaBody, usage: Option<WireUsage> },
    #[serde(rename = "message_stop")]
    MessageStop,
    /// Heartbeat / keep-alive; carries no payload.
    #[serde(rename = "ping")]
    Ping,
}

/// The assistant message as it appears in `message_start`.
#[derive(Debug, Clone, Deserialize)]
pub struct WireMessage {
    pub id: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub content: Vec<WireContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<WireUsage>,
}

/// A content block as it appears in `content_block_start`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum WireContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: JsonValue,
    },
    /// Any block type we don't model (server tools, citations, ...). Kept so
    /// an unknown block doesn't fail the whole stream.
    #[serde(other)]
    Other,
}

/// The incremental payload of a `content_block_delta`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum WireDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageDeltaBody {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

/// Token usage as reported by the API.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_creation: Option<WireCacheCreation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireCacheCreation {
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
}

// ── Error body ──────────────────────────────────────────────────────────────

/// The error envelope returned on non-2xx responses.
#[derive(Debug, Clone, Deserialize)]
pub struct WireError {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub error: WireErrorBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireErrorBody {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}
