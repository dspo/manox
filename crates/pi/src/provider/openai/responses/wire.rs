// Wire types for the OpenAI Responses API (`POST /responses`).
//
// These types mirror the request/response schema field-for-field. The input
// side distinguishes three item families: convenient input messages
// (`{role, content}`, no `type` field), replayed output items (`type:
// message | function_call | function_call_output`), and reasoning items,
// which stay raw JSON so they round-trip byte-for-byte. The stream side is a
// flat event sequence distinguished by a `type` tag; events are probed as
// generic JSON first so unknown event kinds can be ignored rather than fail.
// Nothing outside `provider::openai` should name these types; translation to
// and from the domain types lives in `translate.rs`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ── Request ─────────────────────────────────────────────────────────────────

/// `POST /responses` request body.
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesParams {
    pub model: String,
    pub input: Vec<InputItem>,
    pub stream: bool,
    /// Continuity is client-side: every request replays the full input, so
    /// the server must not retain the response.
    pub store: bool,
    /// Output limit. The API rejects values below 16; the clamp is applied
    /// by the caller of this wire type (see `translate`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolParam>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningParam>,
    /// Side-channel payloads to attach (`reasoning.encrypted_content` makes
    /// reasoning items replayable under `store: false`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<&'static str>>,
    /// Cache-affinity key routing requests of one session to the same
    /// provider-side cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Extended cache retention, when the caller opts in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<&'static str>,
}

/// The reasoning dial. Thinking-capable models always carry this object —
/// `effort: "none"` is the explicit off state, any other level turns
/// reasoning on and takes `summary` alongside.
#[derive(Debug, Clone, Serialize)]
pub struct ReasoningParam {
    pub effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<&'static str>,
}

/// A function tool declaration. The Responses shape inlines the fields that
/// Chat Completions nests under a `function` key.
#[derive(Debug, Clone, Serialize)]
pub struct ToolParam {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function"
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: JsonValue,
}

/// One entry of the request `input` array.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum InputItem {
    /// A convenient-form input message: `{role, content}`, no `type` field.
    Message(InputMessage),
    /// A replayed output item, tagged by `type`.
    Item(OutputItem),
    /// A replayed reasoning item. Kept as raw JSON: the server echoes these
    /// items back verbatim and validates their pairing with function calls,
    /// so any re-encoding drift breaks replay.
    Reasoning(JsonValue),
}

/// `{role, content}` input message. `role` is `developer` for
/// thinking-capable models, `system` or `user` otherwise.
#[derive(Debug, Clone, Serialize)]
pub struct InputMessage {
    pub role: &'static str,
    pub content: InputMessageContent,
}

/// System/developer content is a plain string; user content is always the
/// parts encoding (the protocol's canonical user shape).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum InputMessageContent {
    Text(String),
    Parts(Vec<InputPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum InputPart {
    #[serde(rename = "input_text")]
    Text { text: String },
    #[serde(rename = "input_image")]
    Image {
        image_url: String,
        detail: &'static str,
    },
}

/// A replayed output item (request direction).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    /// An assistant text message from a prior turn. The `id` must be stable
    /// across replays and ≤ 64 chars; `phase` marks commentary vs final
    /// answer when the original item carried it.
    #[serde(rename = "message")]
    Message {
        id: String,
        role: &'static str, // always "assistant"
        content: Vec<OutputTextPart>,
        status: &'static str, // always "completed"
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    /// A function call from a prior turn. `id` pairs the call with the
    /// server's reasoning bookkeeping; it is kept only for same-model
    /// `fc_` ids and omitted otherwise (a stale id fails pairing
    /// validation, an absent one passes).
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        /// The arguments object, serialized to a string as the schema
        /// requires.
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// A tool result. The schema has no error flag and no reasoning about
    /// the call — output content only.
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: FunctionOutput,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputTextPart {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "output_text"
    pub text: String,
    /// Always empty on replay; the field is required by the schema.
    pub annotations: Vec<JsonValue>,
}

/// Tool result output: a plain string, or parts when images are present.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum FunctionOutput {
    Text(String),
    Parts(Vec<InputPart>),
}

// ── SSE stream ──────────────────────────────────────────────────────────────
//
// Events arrive as one JSON object per `data:` payload, discriminated by the
// `type` field. Each payload is probed as generic JSON and then decoded into
// one of the structs below by kind; unknown kinds are skipped.

/// `response.output_item.added` / `response.output_item.done`. The item is
/// left generic: reasoning items must round-trip raw, and the other kinds
/// are decoded on demand via [`WireOutputMessage`] / [`WireFunctionCall`].
#[derive(Debug, Clone, Deserialize)]
pub struct WireOutputItemEvent {
    pub output_index: usize,
    pub item: JsonValue,
}

/// Any delta event keyed by output index (`response.output_text.delta`,
/// `response.refusal.delta`, `response.reasoning_text.delta`,
/// `response.reasoning_summary_text.delta`,
/// `response.function_call_arguments.delta`).
#[derive(Debug, Clone, Deserialize)]
pub struct WireDeltaEvent {
    pub output_index: usize,
    #[serde(default)]
    pub delta: String,
}

/// `response.reasoning_summary_part.done` — carries no payload beyond the
/// index; the part boundary itself is the signal (a `\n\n` separator).
#[derive(Debug, Clone, Deserialize)]
pub struct WireIndexEvent {
    pub output_index: usize,
}

/// `response.function_call_arguments.done` — the authoritative full
/// arguments string, replacing whatever the deltas accumulated.
#[derive(Debug, Clone, Deserialize)]
pub struct WireArgumentsDoneEvent {
    pub output_index: usize,
    #[serde(default)]
    pub arguments: String,
}

/// `response.created` / `response.completed` / `response.incomplete` /
/// `response.failed` — all carry a `response` object.
#[derive(Debug, Clone, Deserialize)]
pub struct WireResponseEvent {
    pub response: WireResponse,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireResponse {
    #[serde(default)]
    pub id: Option<String>,
    /// Model the upstream routed to, reported on `response.created` (and
    /// echoed on terminal events). `None` when the endpoint omits it.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// Present on terminal events; used to backfill reasoning signatures a
    /// mid-stream `output_item.done` may have lacked.
    #[serde(default)]
    pub output: Vec<JsonValue>,
    #[serde(default)]
    pub error: Option<WireResponseError>,
    #[serde(default)]
    pub incomplete_details: Option<WireIncompleteDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireResponseError {
    #[serde(default)]
    pub code: Option<JsonValue>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireIncompleteDetails {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Token usage. `input_tokens` INCLUDES the cached and cache-write subsets.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub input_tokens_details: Option<WireInputTokensDetails>,
    /// Breakdown of output tokens, when the endpoint reports it.
    #[serde(default)]
    pub output_tokens_details: Option<WireOutputTokensDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireInputTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

/// Output-token breakdown. `reasoning_tokens` is the subset of
/// `output_tokens` spent on reasoning, when the endpoint reports it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireOutputTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

/// A `message` output item (decode direction, from `WireOutputItemEvent`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireOutputMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub content: Vec<WireOutputContent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireOutputContent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub refusal: Option<String>,
}

/// A `function_call` output item (decode direction).
#[derive(Debug, Clone, Deserialize)]
pub struct WireFunctionCall {
    pub call_id: String,
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

/// A top-level `error` event in the stream.
#[derive(Debug, Clone, Deserialize)]
pub struct WireErrorEvent {
    #[serde(default)]
    pub code: Option<JsonValue>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Some endpoints answer 2xx and then stream a bare error object (no event
/// `type`) as data. The payload is kept whole for the error message.
#[derive(Debug, Clone, Deserialize)]
pub struct WireErrorPayload {
    pub error: JsonValue,
}
