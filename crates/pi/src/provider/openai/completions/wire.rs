// Wire types for the OpenAI Chat Completions API (`POST /chat/completions`).
//
// These types mirror the request/response schema field-for-field, plus the
// de-facto extensions the ecosystem converged on (streamed reasoning under
// several spellings, flat cache-hit counters). Every extension is optional
// and parsed unconditionally. Nothing outside `provider::openai` should name
// these types; translation to and from the domain types lives in
// `translate.rs`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ── Request ─────────────────────────────────────────────────────────────────

/// `POST /chat/completions` request body.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionParams {
    pub model: String,
    pub messages: Vec<MessageParam>,
    pub stream: bool,
    /// Legacy output-limit field, filled only for the endpoints that accept
    /// nothing else. Mutually exclusive with `max_completion_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    /// The current output-limit field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolParam>>,
    /// The switch-style thinking toggle that `ThinkingKind::Enabled` encodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingParam>,
    /// Dial-style reasoning effort; the level string passes through
    /// unvalidated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Cache-affinity key routing requests of one session to the same
    /// provider-side cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Extended cache retention, when the caller opts in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<&'static str>,
    pub stream_options: WireStreamOptions,
}

/// Streaming flags. `include_usage` makes the endpoint emit a final
/// usage-bearing chunk; without it many compatible endpoints report no
/// billable tokens at all.
#[derive(Debug, Clone, Serialize)]
pub struct WireStreamOptions {
    pub include_usage: bool,
}

/// The `thinking` request field: a bare on/off switch with no depth control.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ThinkingParam {
    #[serde(rename = "enabled")]
    Enabled,
    #[serde(rename = "disabled")]
    Disabled,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolParam {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function"
    pub function: FunctionParam,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionParam {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: JsonValue,
}

/// A message as sent to the API (request direction).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role")]
pub enum MessageParam {
    #[serde(rename = "system")]
    System { content: String },
    #[serde(rename = "user")]
    User { content: UserContent },
    /// `content` is always a plain string, never an array of parts — some
    /// endpoints mirror a parts structure back into their generated output.
    /// Prior reasoning is never replayed as content; `reasoning_content`
    /// exists only for endpoints that require the field present (empty is
    /// accepted).
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCallParam>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    /// A tool result. The schema has no error flag; the error bit folds into
    /// the content as an `[error] ` prefix.
    #[serde(rename = "tool")]
    Tool { tool_call_id: String, content: String },
}

/// User content: a plain string, or parts when images are present.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<UserPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum UserPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlParam },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageUrlParam {
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallParam {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function"
    pub function: ToolCallFunctionParam,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallFunctionParam {
    pub name: String,
    /// The arguments object, serialized to a string as the schema requires.
    pub arguments: String,
}

// ── SSE stream ──────────────────────────────────────────────────────────────

/// One `data:` payload from the streaming endpoint. The stream terminates
/// with a literal `data: [DONE]` line.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireChunk {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    /// Present on the final chunk when `include_usage` was requested.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireChoice {
    #[serde(default)]
    pub delta: Option<WireDelta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Some endpoints report usage per-choice instead of on the chunk.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// Streamed reasoning, in every spelling the ecosystem uses. At most one
    /// spelling carries content in any given chunk; the first non-empty wins.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_text: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<WireToolCallDelta>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireFunctionDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    /// Argument JSON, streamed in fragments.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Token usage. `prompt_tokens` INCLUDES the cached subset; the hit count
/// itself arrives either flat or nested under `prompt_tokens_details`,
/// depending on the endpoint.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WirePromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// Some endpoints answer 2xx and then stream an error object as data. The
/// body shape varies; the payload is kept whole for the error message.
#[derive(Debug, Clone, Deserialize)]
pub struct WireErrorPayload {
    pub error: JsonValue,
}
