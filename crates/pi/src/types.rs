// Core types for the Pi agent harness.
//
// These types form the foundation of the agent loop, defining the message
// structure, event system, context, and configuration that the loop operates on.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

// ── Message types ───────────────────────────────────────────────────────────

/// A content block within a message sent to or received from an LLM.
///
/// Serde tags mirror the Anthropic Messages API wire format so that blocks
/// round-trip without a lossy translation layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        /// Opaque provider data that must be echoed back verbatim on later
        /// turns to preserve the block's server-side identity (item id and
        /// phase for the OpenAI Responses API). `None` for providers whose
        /// protocol carries no text identity, such as Anthropic and Chat
        /// Completions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
    },
    /// A model reasoning trace. `signature` is opaque provider data that must
    /// be echoed back verbatim on later turns to preserve thinking continuity.
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// An encrypted reasoning trace whose content the provider redacted.
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
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
        /// The wire API shape used for this turn ("anthropic",
        /// "openai_completions", "openai_responses", ...). Distinct from
        /// `provider`, which names the vendor.
        api: String,
        /// Concrete `chunk.model` when the upstream returns a different one
        /// than requested (e.g. OpenRouter `auto` -> `anthropic/...`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_model: Option<String>,
        /// Provider-specific response/message identifier when exposed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// Redacted provider/runtime diagnostics for failures and recoveries.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostics: Option<Vec<JsonValue>>,
        /// The stop reason reported for this turn. `None` only while the
        /// message is still streaming; a finalized message always carries one
        /// — `Error`/`Aborted` cover provider failures and local interrupts.
        #[serde(default)]
        stop_reason: Option<StopReason>,
        /// Boxed so the `Assistant` variant — by far the largest, carrying the
        /// provider's full response payload — stays off the enum's inline size.
        /// `Box<Usage>` derefs to `Usage`, so reads are transparent.
        #[serde(default = "default_usage")]
        usage: Box<Usage>,
        /// Failure explanation when `stop_reason` is `Error`/`Aborted`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
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
                signature: None,
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

/// Why the assistant stopped generating, as reported by the provider or set
/// locally on interruption.
///
/// Mirrors the TS Pi `StopReason`: `Stop`/`Length`/`ToolUse` come from the
/// provider's protocol stop reason; `Error` covers provider-reported failures
/// (refusal, content filter, context-window overflow) and transport errors;
/// `Aborted` covers user/system cancellation. An `Error`/`Aborted` message
/// carries `error_message`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

/// Token usage for a single assistant message.
///
/// Field names mirror the Anthropic Messages API usage object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Breakdown of cache creation by TTL, when the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    /// Reasoning/thinking tokens, when the provider reports them. A subset
    /// of `output_tokens`; `None` when the provider exposes no breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Total context tokens. Providers that report a total (Responses) use
    /// it verbatim; the other shapes compute the sum of all token classes
    /// at the wire boundary. Zero means no usage was reported at all.
    #[serde(default)]
    pub total_tokens: u64,
    /// Monetary cost for this response, when a rate card was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

/// Cache creation split by TTL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheCreation {
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
}

/// Monetary cost broken down by token class.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub total: f64,
}

impl Usage {
    /// Total input tokens: direct input plus cache reads and writes.
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

/// Serde default for the boxed `usage` field on `Assistant`.
fn default_usage() -> Box<Usage> {
    Box::new(Usage::default())
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
    MessageStart { message: Box<AgentMessage> },
    /// A streaming message received an update delta.
    MessageUpdate { message: Box<AgentMessage> },
    /// A message has finished streaming.
    MessageEnd { message: Box<AgentMessage> },
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
    ToolExecutionEnd { tool_call_id: String },
    /// A turn has completed.
    TurnEnd {
        message: Box<AgentMessage>,
        tool_results: Vec<AgentMessage>,
    },
    /// A provider handshake failed transiently and is being retried. Emitted
    /// between attempts; a stream that fails after events were already
    /// forwarded is never retried.
    Retry {
        /// 1-indexed attempt that just failed.
        attempt: u32,
        /// Total attempt budget including the original attempt.
        max_attempts: u32,
        /// Delay before the next attempt.
        delay: std::time::Duration,
        /// Short human label, e.g. "429 Too Many Requests" or "connection reset".
        reason: String,
        /// Truncated provider error body, when the failure carried one.
        detail: Option<String>,
    },
    /// The agent run has completed.
    AgentEnd {
        /// All new messages produced during this run.
        messages: Vec<AgentMessage>,
    },
}

/// Sink for agent lifecycle events emitted during the loop and the tool
/// execution pipeline. Implementations forward events to subscribers or
/// capture them in tests.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
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
    /// Maximum output tokens the model can produce per response.
    pub max_tokens: usize,
    /// How the model handles reasoning/thinking.
    pub thinking: ThinkingKind,
    /// Arbitrary provider-specific metadata.
    pub metadata: HashMap<String, JsonValue>,
}

/// How a model handles reasoning.
///
/// Distinguishes the two "thinking on" wire shapes: adaptive models take an
/// effort tier and decide their own depth; enabled models reason when switched
/// on but take no effort-independent budget (depth via `output_config.effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingKind {
    /// No reasoning support — the thinking field is never sent.
    #[default]
    None,
    /// `thinking: {type: "enabled"}` switches reasoning on.
    Enabled,
    /// `thinking: {type: "adaptive"}` — the model decides when/how much.
    Adaptive,
}

impl Model {
    /// Whether any form of thinking can be requested for this model.
    pub fn supports_thinking(&self) -> bool {
        !matches!(self.thinking, ThinkingKind::None)
    }
}

/// Prompt cache retention preference.
///
/// Providers map this to their supported values: the Anthropic shape marks
/// `cache_control` breakpoints ("long" adds `ttl:"1h"`); the OpenAI shapes
/// forward `session_id` as `prompt_cache_key` and send
/// `prompt_cache_retention: "24h"` on "long".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    /// No caching — no cache markers are sent.
    None,
    /// Ephemeral caching (Anthropic default TTL; OpenAI automatic cache).
    #[default]
    Short,
    /// Extended retention (Anthropic `ttl:"1h"`; OpenAI `"24h"`).
    Long,
}

/// The context passed into the agent loop at the start of each turn.
#[derive(Clone)]
pub struct AgentContext {
    /// The current system prompt.
    pub system_prompt: String,
    /// All messages in the conversation (including historical).
    pub messages: Vec<AgentMessage>,
    /// Tools available to the agent. Shared via `Arc` so cloning the context
    /// (notably across the `tokio::spawn` boundary in the stream path) keeps
    /// the tool list intact and the provider sees what the caller mounted.
    pub tools: Arc<[Box<dyn super::AgentTool>]>,
    /// The model being used for this turn.
    pub model: Model,
    /// Current thinking level.
    pub thinking_level: Option<String>,
    /// Prompt cache retention preference for this turn.
    pub cache_retention: CacheRetention,
    /// Session identifier forwarded to providers that support session-based
    /// caching (`prompt_cache_key`). Ignored by providers that don't.
    pub session_id: Option<String>,
    /// Additional context metadata.
    pub metadata: HashMap<String, JsonValue>,
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentContext")
            .field("system_prompt", &self.system_prompt)
            .field("messages", &self.messages)
            .field("tools_count", &self.tools.len())
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Supplies queued messages to inject into the run.
pub type MessageQueueFn = Box<dyn Fn() -> Vec<AgentMessage> + Send + Sync>;
/// Refreshes the context/model before a turn; `None` keeps the current turn.
pub type PrepareTurnFn = Box<dyn Fn(&mut AgentContext) -> Option<AgentContext> + Send + Sync>;
/// Decides whether the run should stop after a turn.
pub type StopAfterTurnFn = Box<dyn Fn(&AgentMessage, &[AgentMessage]) -> bool + Send + Sync>;
/// Gates a tool call before execution; `Some(reason)` blocks it.
pub type BeforeToolCallFn = Box<dyn Fn(&str, &str, &JsonValue) -> Option<String> + Send + Sync>;
/// Patches a tool result after execution.
pub type AfterToolCallFn = Box<dyn Fn(&AgentToolResult) -> AgentToolResult + Send + Sync>;

/// Configuration for a single agent loop invocation.
#[derive(Default)]
pub struct AgentLoopConfig {
    /// Callback to get queued steering messages (injected mid-turn).
    pub get_steering_messages: Option<MessageQueueFn>,
    /// Callback to get follow-up messages (injected after turn settles).
    pub get_follow_up_messages: Option<MessageQueueFn>,
    /// Called before each turn to potentially refresh context/model.
    pub prepare_next_turn: Option<PrepareTurnFn>,
    /// Called after each turn to decide whether to stop.
    pub should_stop_after_turn: Option<StopAfterTurnFn>,
    /// Called before a tool call executes. Return `Some(reason)` to block.
    pub before_tool_call: Option<BeforeToolCallFn>,
    /// Called after a tool call executes to patch the result.
    pub after_tool_call: Option<AfterToolCallFn>,
    /// Whether tools execute sequentially (default: parallel).
    pub sequential_tool_execution: bool,
    /// Maximum number of turns before forcing a stop.
    pub max_turns: Option<usize>,
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
///
/// Cache preferences are NOT carried here: they are per-conversation state
/// owned by [`AgentContext::cache_retention`] / [`AgentContext::session_id`].
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// Maximum output tokens.
    pub max_tokens: Option<usize>,
    /// Temperature override.
    pub temperature: Option<f32>,
}

// ── Re-export from tool module ──────────────────────────────────────────────

use super::tool::AgentToolResult;
