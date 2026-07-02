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

/// A single message content block. The first version carries only Text / Thinking / ToolUse / ToolResult.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse(LanguageModelToolUse),
    ToolResult(LanguageModelToolResult),
}

impl MessageContent {
    pub fn to_str(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text.as_str()),
            Self::Thinking { text, .. } => Some(text.as_str()),
            Self::ToolResult(result) => Some(result.content.as_str()),
            Self::ToolUse(_) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) | Self::Thinking { text, .. } => text.chars().all(|c| c.is_whitespace()),
            Self::ToolResult(result) => result.content.chars().all(|c| c.is_whitespace()),
            Self::ToolUse(_) => false,
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
    UsageUpdate(TokenUsage),
    Stop(StopReason),
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
        Self {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens
                + other.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens + other.cache_read_input_tokens,
        }
    }
}

/// Language model abstraction.
pub trait LanguageModel: Send + Sync {
    fn id(&self) -> String;
    fn name(&self) -> String;
    fn provider_id(&self) -> String;
    fn provider_name(&self) -> String;

    fn supports_thinking(&self) -> bool {
        false
    }
    fn supports_tools(&self) -> bool {
        true
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
