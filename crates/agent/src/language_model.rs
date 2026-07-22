//! `LanguageModel` trait and request/response types.
//!
//! Uses `String` instead of a `SharedString` newtype; the first version omits
//! `Image` / `RedactedThinking`. Provider implementations live under `provider::`.

use std::sync::Arc;

use futures::{future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};
use tracing::warn;

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// A single message content block: text, thinking, an image, or a tool call/result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    /// An inline image carried as base64-encoded bytes plus its MIME type, sent to
    /// providers that accept multimodal content blocks.
    Image {
        data: String,
        mime_type: String,
    },
    ToolUse(LanguageModelToolUse),
    ToolResult(LanguageModelToolResult),
    /// A context-compaction summary replacing older history. Carried by a
    /// `Role::User` message; never sent to the provider verbatim —
    /// `model_facing_content` rewrites it into a `Text` block wrapped with an
    /// explanatory preamble so the model treats it as user-supplied context.
    /// The full summary survives in `Thread::messages` for persistence and UI
    /// rebuild; only one is inserted per compaction pass.
    Compaction(String),
}

impl MessageContent {
    pub fn to_str(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text.as_str()),
            Self::Thinking { text, .. } => Some(text.as_str()),
            Self::ToolResult(result) => Some(result.content.as_str()),
            Self::Compaction(text) => Some(text.as_str()),
            Self::Image { .. } | Self::ToolUse(_) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) | Self::Thinking { text, .. } | Self::Compaction(text) => {
                text.chars().all(|c| c.is_whitespace())
            }
            Self::ToolResult(result) => result.content.chars().all(|c| c.is_whitespace()),
            Self::Image { data, .. } => data.is_empty(),
            Self::ToolUse(_) => false,
        }
    }

    /// The compaction summary text, if this is a `Compaction` block.
    pub fn compaction_summary(&self) -> Option<&str> {
        match self {
            Self::Compaction(text) => Some(text.as_str()),
            _ => None,
        }
    }
}

impl From<String> for MessageContent {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for MessageContent {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

/// A tool call issued by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageModelToolUse {
    pub id: String,
    pub name: Arc<str>,
    pub raw_input: String,
    pub input: serde_json::Value,
    pub is_input_complete: bool,
    /// Thinking signature; some models require it echoed back for verification.
    pub thought_signature: Option<String>,
}

/// A tool result sent back to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageModelToolResult {
    pub tool_use_id: String,
    pub tool_name: Arc<str>,
    pub is_error: bool,
    /// Tool output shown to the model (text-only in the first version).
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageModelRequestMessage {
    pub role: Role,
    pub content: Vec<MessageContent>,
    pub cache: bool,
}

impl LanguageModelRequestMessage {
    pub fn string_contents(&self) -> String {
        let mut buffer = String::new();
        for string in self.content.iter().filter_map(MessageContent::to_str) {
            buffer.push_str(string);
        }
        buffer
    }

    pub fn contents_empty(&self) -> bool {
        self.content.iter().all(MessageContent::is_empty)
    }
}

/// A tool definition sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageModelRequestTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub use_input_streaming: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LanguageModelToolChoice {
    Auto,
    Any,
    None,
}

/// User-facing reasoning effort knob for providers that expose an effort
/// parameter (Anthropic `output_config.effort`, OpenAI `reasoning.effort`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    #[default]
    High,
    Max,
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "high" => Self::High,
            "max" => Self::Max,
            other => {
                warn!(
                    effort = other,
                    "unknown reasoning_effort in config; falling back to High"
                );
                Self::High
            }
        })
    }
}

impl ReasoningEffort {
    pub const ALL: [Self; 2] = [Self::High, Self::Max];

    /// Wire value for Anthropic-style and generic providers.
    pub fn wire_value(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Wire value for official OpenAI endpoints, which only accept
    /// `low` / `medium` / `high`. `Max` is clamped to `high` on official
    /// OpenAI; compatible endpoints pass `max` through.
    pub fn openai_wire_value(self, official_openai: bool) -> &'static str {
        if official_openai && matches!(self, Self::Max) {
            "high"
        } else {
            self.wire_value()
        }
    }

    /// Integer encoding for SQLite persistence. Keeps the original code points
    /// (`High` = 2, `Max` = 4) for backward compatibility with existing DB
    /// records; unknown values fall back to `High`.
    pub fn from_i64(v: i64) -> Self {
        match v {
            2 => Self::High,
            4 => Self::Max,
            _ => Self::High,
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Self::High => 2,
            Self::Max => 4,
        }
    }
}

/// A single completion request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LanguageModelRequest {
    pub messages: Vec<LanguageModelRequestMessage>,
    pub tools: Vec<LanguageModelRequestTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<LanguageModelToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Advisory hint that non-interactive generation paths (title, compact,
    /// goal, approval) would prefer the model not spend a thinking budget
    /// before its text reply. NOT translated to any provider's wire format
    /// here: there is no universal value. Anthropic's `thinking:{type:
    /// "disabled"}` 400s on adaptive-only models (Opus 4.7+ / Sonnet 5) which
    /// only accept `{type:"adaptive",effort}`, while that `adaptive` shape 400s
    /// on classic thinking models (3.7 / 4.0 / 4.6). Translating it correctly
    /// needs a per-model capability signal, which the provider layer does not
    /// expose yet; until then the field is advisory-only and title-generation
    /// failures are surfaced via `tracing::warn!` in `title_state`.
    #[serde(default)]
    pub thinking_allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// A streaming completion event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LanguageModelCompletionEvent {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse(LanguageModelToolUse),
    ToolUseJsonParseError {
        id: String,
        tool_name: Arc<str>,
        raw_input: String,
        json_parse_error: String,
    },
    /// Running-total token usage for the in-flight request. The payload is the
    /// cumulative usage for the whole request so far (input grows as the prefix
    /// is cached, output grows as tokens stream), NOT a per-event delta — the
    /// consumer takes `max()` against its own running total and derives the
    /// delta via `saturating_sub` against the previous total (see
    /// `Thread::accumulate_token_usage`). Providers must emit this before
    /// `Stop` whenever they received a usage payload, including on terminal
    /// non-success events (e.g. `response.incomplete`), so the turn's tokens
    /// are not silently lost.
    UsageUpdate(TokenUsage),
    Stop(StopReason),
    /// Provider is retrying the HTTP handshake after a transient failure
    /// (429 / 5xx / network error). Emitted before each backoff sleep so the
    /// UI can surface a retry badge; the next non-`Retry` event resolves it.
    /// `reason` is a short user-facing label (HTTP status phrase or a network
    /// error class); `detail` carries the truncated provider response body for
    /// the expandable card, `None` for network errors.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refusal,
    /// The user explicitly interrupted the active turn. Kept distinct from a
    /// natural `EndTurn` so queued follow-ups are not auto-submitted.
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "is_default")]
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_default")]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_default")]
    pub cache_creation_input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_default")]
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }
}

impl std::ops::Add for TokenUsage {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        // saturating_add: token counts are u64 and only ever grow in
        // practice, but a corrupted/overflowing provider payload should not
        // panic in debug or silently wrap in release.
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }
}

use crate::provider::WireApi;

/// Language model abstraction.
pub trait LanguageModel: Send + Sync {
    fn id(&self) -> String;
    fn name(&self) -> String;
    fn provider_id(&self) -> String;
    fn provider_name(&self) -> String;
    fn wire_api(&self) -> WireApi;

    /// cx agent ids this model can drive (`claude` / `codex` / `copilot` / …),
    /// sourced from the provider config's endpoint `agents:` list. Empty means
    /// no external-agent coupling — a plain manox-thread model. The new-session
    /// wizard filters the model list by the chosen agent's id.
    fn visible_agents(&self) -> &[String] {
        &[]
    }

    fn supports_thinking(&self) -> bool {
        false
    }
    fn supports_tools(&self) -> bool {
        true
    }
    /// Whether the model accepts image attachments in user content. Sourced
    /// from the provider config's per-model `supports_images` field (ground
    /// truth, not model self-report). Defaults to `false`; concrete providers
    /// override when the resolved model declares the capability.
    fn supports_images(&self) -> bool {
        false
    }
    /// Whether the provider supports long-lived prompt cache retention
    /// (`cache_control.ttl:"1h"` on Anthropic, `prompt_cache_retention:"24h"`
    /// on OpenAI). Defaults to `false`; concrete providers override based on
    /// the endpoint host (official APIs only).
    fn supports_long_prompt_cache_retention(&self) -> bool {
        false
    }
    fn max_token_count(&self) -> u64;
    /// Auto-compact window override (token count), sourced from the provider
    /// config's provider-level or model-level `env: CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
    /// Only effective on the Anthropic wire. When `Some`, the thread
    /// auto-compacts at 80% of this value (Claude Code parity) instead of the
    /// model's full `max_token_count` at the user's settings threshold. Defaults
    /// to `None`; only Anthropic-wire models whose config sets the env var
    /// override it.
    fn auto_compact_window(&self) -> Option<u64> {
        None
    }

    /// Stream a completion. Returns a `BoxFuture` (handshake) that yields a `BoxStream` of events.
    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        anyhow::Result<BoxStream<'static, anyhow::Result<LanguageModelCompletionEvent>>>,
    >;
}

pub type AnyLanguageModel = Arc<dyn LanguageModel>;

#[cfg(test)]
mod tests {
    use super::ReasoningEffort;

    #[test]
    fn wire_values_match_provider_expectations() {
        assert_eq!(ReasoningEffort::High.wire_value(), "high");
        assert_eq!(ReasoningEffort::Max.wire_value(), "max");
    }

    #[test]
    fn openai_wire_value_clamps_max_on_official_openai() {
        assert_eq!(ReasoningEffort::Max.openai_wire_value(true), "high");
        // Compatible endpoints pass max through.
        assert_eq!(ReasoningEffort::Max.openai_wire_value(false), "max");
    }

    #[test]
    fn from_i64_is_backward_compatible_and_falls_back_to_high() {
        // Known code points.
        assert_eq!(ReasoningEffort::from_i64(2), ReasoningEffort::High);
        assert_eq!(ReasoningEffort::from_i64(4), ReasoningEffort::Max);
        // Unknown values fall back to High.
        assert_eq!(ReasoningEffort::from_i64(0), ReasoningEffort::High);
        assert_eq!(ReasoningEffort::from_i64(99), ReasoningEffort::High);
    }

    #[test]
    fn as_i64_round_trips() {
        for effort in ReasoningEffort::ALL {
            assert_eq!(ReasoningEffort::from_i64(effort.as_i64()), effort);
        }
    }

    #[test]
    fn deserialize_unknown_variant_falls_back_to_high() {
        // Old settings.toml values must not break the entire config parse.
        for (input, expected) in [
            ("high", ReasoningEffort::High),
            ("max", ReasoningEffort::Max),
            // Removed variants: each must fall back to High.
            ("low", ReasoningEffort::High),
            ("medium", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::High),
            ("ultracode", ReasoningEffort::High),
            ("auto", ReasoningEffort::High),
            ("garbage", ReasoningEffort::High),
        ] {
            let v: ReasoningEffort = serde_json::from_str(&format!("\"{input}\""))
                .unwrap_or_else(|e| panic!("deserialize \"{input}\" must succeed, got: {e}"));
            assert_eq!(
                v, expected,
                "\"{input}\" must resolve to {:?}, got {:?}",
                expected, v
            );
        }
    }
}
