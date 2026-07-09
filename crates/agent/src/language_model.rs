//! `LanguageModel` trait and request/response types.
//!
//! Uses `String` instead of a `SharedString` newtype; the first version omits
//! `Image` / `RedactedThinking`. Provider implementations live under `provider::`.

use std::sync::Arc;

use futures::{future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};

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
/// `Auto` and `Ultracode` are not API values — `resolve_for_wire` folds them
/// into a concrete level before the request reaches any provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    #[default]
    High,
    XHigh,
    Max,
    Ultracode,
    Auto,
}

impl ReasoningEffort {
    pub const ALL: [Self; 7] = [
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultracode,
        Self::Auto,
    ];

    /// Wire value for Anthropic-style and generic providers.
    pub fn wire_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultracode => "ultracode",
            Self::Auto => "auto",
        }
    }

    /// Resolve `Auto` / `Ultracode` into a concrete level the wire accepts.
    /// Providers only ever receive the resolved value: `Auto` picks the
    /// per-context default (main thread, depth 0 → `High`; sub-agent →
    /// `Medium`, floored so it never drops below `Medium`), and `Ultracode`
    /// resolves to `XHigh` for the wire while its multi-agent grant is a
    /// separate system-message side effect handled in `build_completion_request`.
    pub fn resolve_for_wire(self, depth: u32) -> Self {
        match self {
            Self::Auto => {
                if depth == 0 {
                    Self::High
                } else {
                    Self::Medium
                }
            }
            Self::Ultracode => Self::XHigh,
            other => other,
        }
    }

    /// Wire value for official OpenAI endpoints, which only accept
    /// `low` / `medium` / `high`. Everything above `high` is clamped. `Auto`
    /// never reaches a provider — `resolve_for_wire` folds it away first —
    /// so it is absent from the non-clamp set and would clamp to `high` if
    /// it ever did reach this call.
    pub fn openai_wire_value(self, official_openai: bool) -> &'static str {
        if official_openai && !matches!(self, Self::Low | Self::Medium | Self::High) {
            "high"
        } else {
            self.wire_value()
        }
    }

    /// Integer encoding for SQLite persistence. Mirrors `ApprovalMode::as_i64`.
    pub fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::Low,
            1 => Self::Medium,
            2 => Self::High,
            3 => Self::XHigh,
            4 => Self::Max,
            5 => Self::Ultracode,
            6 => Self::Auto,
            _ => Self::High,
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::XHigh => 3,
            Self::Max => 4,
            Self::Ultracode => 5,
            Self::Auto => 6,
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
    /// Carries no error text — raw error strings stay in tracing logs and out
    /// of user-facing UI.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refusal,
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

    fn supports_thinking(&self) -> bool {
        false
    }
    fn supports_tools(&self) -> bool {
        true
    }
    /// Whether the provider supports long-lived prompt cache retention
    /// (`cache_control.ttl:"1h"` on Anthropic, `prompt_cache_retention:"24h"`
    /// on OpenAI). Defaults to `false`; concrete providers override based on
    /// the endpoint host (official APIs only).
    fn supports_long_prompt_cache_retention(&self) -> bool {
        false
    }
    fn max_token_count(&self) -> u64;

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
    fn resolve_for_wire_auto_uses_high_for_main_thread() {
        // Main thread (depth 0) → High; the floor is Medium, but the main
        // agent default is High.
        assert_eq!(
            ReasoningEffort::Auto.resolve_for_wire(0),
            ReasoningEffort::High
        );
    }

    #[test]
    fn resolve_for_wire_auto_uses_medium_for_subagent() {
        // Sub-agents step down to Medium and never drop below it.
        assert_eq!(
            ReasoningEffort::Auto.resolve_for_wire(1),
            ReasoningEffort::Medium
        );
        assert_eq!(
            ReasoningEffort::Auto.resolve_for_wire(5),
            ReasoningEffort::Medium
        );
    }

    #[test]
    fn resolve_for_wire_ultracode_folds_to_xhigh() {
        // Ultracode's wire value is xhigh; the multi-agent grant is a separate
        // system-message side effect, not a wire value.
        assert_eq!(
            ReasoningEffort::Ultracode.resolve_for_wire(0),
            ReasoningEffort::XHigh
        );
        assert_eq!(
            ReasoningEffort::Ultracode.resolve_for_wire(3),
            ReasoningEffort::XHigh
        );
    }

    #[test]
    fn resolve_for_wire_passes_concrete_levels_through() {
        for level in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ] {
            assert_eq!(level.resolve_for_wire(0), level);
            assert_eq!(level.resolve_for_wire(2), level);
        }
    }
}
