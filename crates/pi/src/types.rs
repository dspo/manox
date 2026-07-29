// Core types for the Pi agent harness.
//
// These types form the foundation of the agent loop, defining the message
// structure, event system, context, and configuration that the loop operates on.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

// ── Message types ───────────────────────────────────────────────────────────

/// A content block within a message sent to or received from an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: JsonValue,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

/// Source of an image in a content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ImageSource {
    #[serde(rename = "base64")]
    Base64 { media_type: String, data: String },
    #[serde(rename = "url")]
    Url { url: String },
}

/// A message in the agent conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum AgentMessage {
    #[serde(rename = "user")]
    User {
        content: Vec<ContentBlock>,
        #[serde(default = "chrono::Utc::now")]
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        content: Vec<ContentBlock>,
        model: String,
        provider: String,
        #[serde(default)]
        stop_reason: Option<StopReason>,
        #[serde(default)]
        usage: Usage,
        #[serde(default = "chrono::Utc::now")]
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        details: Option<JsonValue>,
        #[serde(default = "chrono::Utc::now")]
        timestamp: DateTime<Utc>,
    },
    /// Extension point for custom message types.
    #[serde(rename = "custom")]
    Custom {
        custom_type: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        details: Option<JsonValue>,
        #[serde(default = "chrono::Utc::now")]
        timestamp: DateTime<Utc>,
    },
}

impl AgentMessage {
    /// Create a user message from plain text.
    pub fn user(text: impl Into<String>) -> Self {
        AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: text.into(),
            }],
            timestamp: Utc::now(),
        }
    }

    /// The timestamp of this message, regardless of variant.
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            AgentMessage::User { timestamp, .. }
            | AgentMessage::Assistant { timestamp, .. }
            | AgentMessage::ToolResult { timestamp, .. }
            | AgentMessage::Custom { timestamp, .. } => *timestamp,
        }
    }
}

/// Why the assistant stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natural completion (end_turn).
    EndTurn,
    /// Token limit reached.
    Length,
    /// Tool call requested.
    ToolUse,
    /// Aborted by user or system.
    Aborted,
    /// Provider error.
    Error,
}

/// Token usage for a single assistant message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub total: u64,
}

// ── Event types ─────────────────────────────────────────────────────────────

/// Events emitted during an agent run.
///
/// These form the complete lifecycle: `agent_start` → repeated `turn_start`
/// → `message_*` → `tool_execution_*` → `turn_end` → `agent_end`.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A new agent run has begun.
    AgentStart,
    /// A new turn has started.
    TurnStart,
    /// A new message has started streaming.
    MessageStart {
        message: Box<AgentMessage>,
    },
    /// A streaming message received an update delta.
    MessageUpdate {
        message: Box<AgentMessage>,
    },
    /// A message has finished streaming.
    MessageEnd {
        message: Box<AgentMessage>,
    },
    /// A tool call has started executing.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
    },
    /// A currently-executing tool emitted an update.
    ToolExecutionUpdate {
        tool_call_id: String,
        details: JsonValue,
    },
    /// A tool call has finished executing.
    ToolExecutionEnd {
        tool_call_id: String,
    },
    /// A turn has completed.
    TurnEnd {
        message: Box<AgentMessage>,
        tool_results: Vec<AgentMessage>,
    },
    /// The agent run has completed.
    AgentEnd {
        /// All new messages produced during this run.
        messages: Vec<AgentMessage>,
    },
}

// ── Agent context and configuration ─────────────────────────────────────────

/// A model descriptor.
#[derive(Debug, Clone)]
pub struct Model {
    /// Provider identifier (e.g. "anthropic", "openai").
    pub provider: String,
    /// Model identifier (e.g. "claude-sonnet-4-6").
    pub id: String,
    /// Maximum context window in tokens.
    pub context_window: usize,
    /// Whether the model supports reasoning/thinking.
    pub supports_thinking: bool,
    /// Arbitrary provider-specific metadata.
    pub metadata: HashMap<String, JsonValue>,
}

/// The context passed into the agent loop at the start of each turn.
pub struct AgentContext {
    /// The current system prompt.
    pub system_prompt: String,
    /// All messages in the conversation (including historical).
    pub messages: Vec<AgentMessage>,
    /// Tools available to the agent (not clonable — trait objects).
    pub tools: Vec<Box<dyn super::AgentTool>>,
    /// The model being used for this turn.
    pub model: Model,
    /// Current thinking level.
    pub thinking_level: Option<String>,
    /// Additional context metadata.
    pub metadata: HashMap<String, JsonValue>,
}

impl Clone for AgentContext {
    fn clone(&self) -> Self {
        AgentContext {
            system_prompt: self.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: Vec::new(), // tools are not cloned — caller must re-set
            model: self.model.clone(),
            thinking_level: self.thinking_level.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentContext")
            .field("system_prompt", &self.system_prompt)
            .field("messages", &self.messages)
            .field("tools_count", &self.tools.len())
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Configuration for a single agent loop invocation.
pub struct AgentLoopConfig {
    /// Callback to get queued steering messages (injected mid-turn).
    pub get_steering_messages: Option<Box<dyn Fn() -> Vec<AgentMessage> + Send + Sync>>,
    /// Callback to get follow-up messages (injected after turn settles).
    pub get_follow_up_messages: Option<Box<dyn Fn() -> Vec<AgentMessage> + Send + Sync>>,
    /// Called before each turn to potentially refresh context/model.
    pub prepare_next_turn:
        Option<Box<dyn Fn(&mut AgentContext) -> Option<AgentContext> + Send + Sync>>,
    /// Called after each turn to decide whether to stop.
    pub should_stop_after_turn:
        Option<Box<dyn Fn(&AgentMessage, &[AgentMessage]) -> bool + Send + Sync>>,
    /// Called before a tool call executes. Return `Some(reason)` to block.
    pub before_tool_call:
        Option<Box<dyn Fn(&str, &str, &JsonValue) -> Option<String> + Send + Sync>>,
    /// Called after a tool call executes to patch the result.
    pub after_tool_call:
        Option<Box<dyn Fn(&AgentToolResult) -> AgentToolResult + Send + Sync>>,
    /// Whether tools execute sequentially (default: parallel).
    pub sequential_tool_execution: bool,
    /// Maximum number of turns before forcing a stop.
    pub max_turns: Option<usize>,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        AgentLoopConfig {
            get_steering_messages: None,
            get_follow_up_messages: None,
            prepare_next_turn: None,
            should_stop_after_turn: None,
            before_tool_call: None,
            after_tool_call: None,
            sequential_tool_execution: false,
            max_turns: None,
        }
    }
}

// ── Agent state ─────────────────────────────────────────────────────────────

/// Mutable state tracked by the Agent struct.
#[derive(Debug, Clone)]
pub struct AgentState {
    /// The system prompt for the current conversation.
    pub system_prompt: String,
    /// The current model.
    pub model: Model,
    /// The current thinking level.
    pub thinking_level: Option<String>,
    /// All messages in the current conversation.
    pub messages: Vec<AgentMessage>,
    /// Whether the agent is currently streaming.
    pub is_streaming: bool,
    /// The message currently being streamed, if any.
    pub streaming_message: Option<AgentMessage>,
    /// IDs of tool calls currently in flight.
    pub pending_tool_calls: Vec<String>,
    /// Error message from the last turn, if any.
    pub error_message: Option<String>,
}

impl AgentState {
    /// Create a new agent state with the given system prompt and model.
    pub fn new(system_prompt: impl Into<String>, model: Model) -> Self {
        AgentState {
            system_prompt: system_prompt.into(),
            model,
            thinking_level: None,
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: Vec::new(),
            error_message: None,
        }
    }
}

// ── Stream options ──────────────────────────────────────────────────────────

/// Options passed to the LLM streaming function.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// Prompt cache retention preference.
    pub cache_retention: Option<String>,
    /// Session identifier for cache affinity.
    pub session_id: Option<String>,
    /// Maximum output tokens.
    pub max_tokens: Option<usize>,
    /// Temperature override.
    pub temperature: Option<f32>,
}

// ── Re-export from tool module ──────────────────────────────────────────────

use super::tool::AgentToolResult;