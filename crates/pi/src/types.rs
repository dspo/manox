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
/// Mirrors the TS Pi content shapes: `text` carries `textSignature`, `image`
/// is flat with `mimeType`, `toolCall` carries `arguments` + `thoughtSignature`,
/// and redacted reasoning is a `thinking` block with `redacted: true` (the
/// opaque payload lives in `thinkingSignature`), not a separate type.
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
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "textSignature"
        )]
        signature: Option<String>,
    },
    #[serde(rename = "image")]
    Image {
        /// Base64-encoded image bytes.
        data: String,
        /// MIME type, e.g. `image/png`.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "toolCall")]
    ToolUse {
        id: String,
        name: String,
        #[serde(rename = "arguments")]
        input: JsonValue,
        /// Opaque provider signature for reusing thought context (Google).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "thoughtSignature"
        )]
        thought_signature: Option<String>,
    },
    /// A model reasoning trace. `signature` is opaque provider data that must
    /// be echoed back verbatim on later turns to preserve thinking continuity.
    /// `redacted` marks a trace the provider encrypted; its payload is stored
    /// in `signature` and `thinking` is empty.
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "thinkingSignature"
        )]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
}

/// A message in the agent conversation.
///
/// Serialized in the TS Pi v3 message shape: roles `user` / `assistant` /
/// `toolResult`, with camelCase fields (`toolCallId`, `toolName`, `isError`,
/// `stopReason`, `responseId`, `responseModel`, `errorMessage`, `customType`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum AgentMessage {
    #[serde(rename = "user", rename_all = "camelCase")]
    User {
        #[serde(default, deserialize_with = "deserialize_content_blocks")]
        content: Vec<ContentBlock>,
        #[serde(default = "chrono::Utc::now", with = "ts_millis")]
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "assistant", rename_all = "camelCase")]
    Assistant {
        /// Content blocks of the turn. Accepts the same string-or-array wire
        /// shapes as user content; a null/missing content reads as empty,
        /// matching how the TS session layer guards damaged entries.
        #[serde(default, deserialize_with = "deserialize_content_blocks")]
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
        /// The provider's raw stop-reason string, kept verbatim for
        /// diagnostics and persistence (TS `rawStopReason`). Only providers
        /// that report one set it — Anthropic's `stop_reason`, Completions'
        /// `finish_reason`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_stop_reason: Option<String>,
        /// Boxed so the `Assistant` variant — by far the largest, carrying the
        /// provider's full response payload — stays off the enum's inline size.
        /// `Box<Usage>` derefs to `Usage`, so reads are transparent.
        #[serde(default = "default_usage")]
        usage: Box<Usage>,
        /// Failure explanation when `stop_reason` is `Error`/`Aborted`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        #[serde(default = "chrono::Utc::now", with = "ts_millis")]
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "toolResult", rename_all = "camelCase")]
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        /// Token usage attributed to the tool result, when the provider reports
        /// per-call usage (e.g. Responses API `usage` on the output item).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Tool names the call added to the session's allowed set, when the
        /// provider reports additions (Responses API `added_tool_names`).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "addedToolNames"
        )]
        added_tool_names: Option<Vec<String>>,
        #[serde(default = "chrono::Utc::now", with = "ts_millis")]
        timestamp: DateTime<Utc>,
    },
    /// A shell command the user ran outside the model's tool calls, recorded so
    /// the transcript reflects what happened in the working tree.
    #[serde(rename = "bashExecution", rename_all = "camelCase")]
    BashExecution {
        command: String,
        /// Combined stdout and stderr, already truncated.
        output: String,
        /// `None` when the process was killed before reporting a status.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        truncated: bool,
        /// Where the untruncated output was spilled, when it was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full_output_path: Option<String>,
        /// Withholds the execution from the model while keeping it in the
        /// session — the transcript records it, the provider never sees it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
        #[serde(default = "chrono::Utc::now", with = "ts_millis")]
        timestamp: DateTime<Utc>,
    },
    /// Extension point for custom message types.
    #[serde(rename = "custom", rename_all = "camelCase")]
    Custom {
        custom_type: String,
        #[serde(default, deserialize_with = "deserialize_content_blocks")]
        content: Vec<ContentBlock>,
        /// Whether the message renders in the UI. Context projection does not
        /// branch on it — a custom message joins the context either way.
        #[serde(default)]
        display: bool,
        #[serde(default)]
        details: Option<JsonValue>,
        #[serde(default = "chrono::Utc::now", with = "ts_millis")]
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
            | AgentMessage::BashExecution { timestamp, .. }
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
/// Serialized in the TS Pi v3 usage shape: `input` / `output` / `cacheRead` /
/// `cacheWrite` / `totalTokens`, with an optional `cacheWrite1h` split and a
/// `cost` breakdown. Rust field names stay snake_case; serde renames map them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default, rename = "input")]
    pub input_tokens: u64,
    #[serde(default, rename = "output")]
    pub output_tokens: u64,
    #[serde(default, rename = "cacheRead")]
    pub cache_read_input_tokens: u64,
    #[serde(default, rename = "cacheWrite")]
    pub cache_creation_input_tokens: u64,
    /// The 1h-TTL portion of cache creation, when the provider reports it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "cacheWrite1h"
    )]
    pub cache_write_1h: Option<u64>,
    /// Reasoning/thinking tokens, when the provider reports them. A subset
    /// of `output_tokens`; `None` when the provider exposes no breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "reasoning")]
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

/// Monetary cost broken down by token class.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

/// Deserialize message content, accepting either a plain string (wrapped in a
/// single `text` block) or an array of content blocks — the two wire shapes
/// TS Pi emits for user-typed and tool/image content. A null content (TS
/// writes `{...message, content: []}` for damaged entries, and hand-edited
/// files may carry null) reads as no blocks.
pub(crate) fn deserialize_content_blocks<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<ContentBlock>, D::Error> {
    use serde::de::Error;

    let value = serde_json::Value::deserialize(d)?;
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::String(s) => Ok(vec![ContentBlock::Text {
            text: s,
            signature: None,
        }]),
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(|item| serde_json::from_value::<ContentBlock>(item).map_err(Error::custom))
            .collect(),
        other => Err(Error::custom(format!(
            "expected string or array of content blocks, got {}",
            other
        ))),
    }
}

/// Serde for `AgentMessage` timestamps as epoch milliseconds — the on-disk
/// shape TS Pi v3 stores for a message's own timestamp (entry-level
/// timestamps stay ISO strings). Used only for session storage; the wire
/// formats build their own request structs and never touch this.
mod ts_millis {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(dt.timestamp_millis())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Number(n) => {
                let ms = n.as_i64().ok_or_else(|| {
                    serde::de::Error::custom("timestamp must be integer milliseconds")
                })?;
                DateTime::<Utc>::from_timestamp_millis(ms)
                    .ok_or_else(|| serde::de::Error::custom("timestamp millis out of range"))
            }
            _ => Err(serde::de::Error::custom(
                "timestamp must be integer milliseconds",
            )),
        }
    }
}

// ── Event types ─────────────────────────────────────────────────────────────

/// Incremental stream event attached to [`AgentEvent::MessageUpdate`].
///
/// Mirrors the TS Pi `AssistantMessageEvent` variants that a `message_update`
/// can carry: `content_index` addresses the block in the partial assistant
/// message's content array, `delta` holds the just-arrived fragment, and the
/// `*_end` variants carry the block's finalized content. The TS `start`,
/// `done`, and `error` variants have no counterpart here — the Rust stream
/// boundary delivers them as the message lifecycle (`MessageStart`) and the
/// stream function's return value instead.
#[derive(Debug, Clone)]
pub enum AssistantMessageEvent {
    /// A text block began streaming.
    TextStart { content_index: usize },
    /// A text block received a fragment.
    TextDelta { content_index: usize, delta: String },
    /// A text block finished; `content` is its full text.
    TextEnd {
        content_index: usize,
        content: String,
    },
    /// A thinking block began streaming.
    ThinkingStart { content_index: usize },
    /// A thinking block received a fragment.
    ThinkingDelta { content_index: usize, delta: String },
    /// A thinking block finished; `content` is its full text.
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    /// A tool call block began streaming.
    ToolCallStart { content_index: usize },
    /// A tool call block received a fragment of its arguments JSON.
    ToolCallDelta { content_index: usize, delta: String },
    /// A tool call block finished; `tool_call` is always the
    /// [`ContentBlock::ToolUse`] variant with resolved arguments.
    ToolCallEnd {
        content_index: usize,
        tool_call: ContentBlock,
    },
}

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
    MessageUpdate {
        message: Box<AgentMessage>,
        assistant_message_event: AssistantMessageEvent,
    },
    /// A message has finished streaming.
    MessageEnd { message: Box<AgentMessage> },
    /// A tool call has started executing. Carries the arguments the model
    /// supplied so consumers can reconstruct the call without walking history.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        arguments: JsonValue,
    },
    /// A currently-executing tool emitted a partial result. Repeats the call
    /// identity and arguments alongside the partial payload so a consumer can
    /// attach progress to the right call without cross-referencing history.
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        arguments: JsonValue,
        partial_result: JsonValue,
    },
    /// A tool call has finished executing. Carries the full result — content,
    /// details, per-call usage, added tool names, and the terminate signal —
    /// alongside a top-level error flag, mirroring the TS Pi event so a
    /// consumer can branch on success without unpacking the result.
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
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
///
/// `emit` is async so a slow consumer backpressures the loop: the loop awaits
/// each emission, so state reduction and subscribed listeners settle before
/// the run advances — the same ordering TS Pi's awaited `emit` provides.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    /// Emit an event. An `Err` aborts the run: a persistence or subscriber
    /// failure must stop further provider/tool effects rather than letting
    /// the conversation diverge from what was durably recorded.
    async fn emit(&self, event: AgentEvent) -> Result<(), anyhow::Error>;
}

// ── Agent context and configuration ─────────────────────────────────────────

/// A model descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Provider identifier (e.g. "anthropic", "openai").
    pub provider: String,
    /// The wire API shape this model speaks (e.g. "anthropic",
    /// "openai_completions", "openai_responses") — the discriminator a
    /// [`StreamResolver`](crate::agent_loop::StreamFn) uses to pick the
    /// provider runtime, mirroring the TS `Model.api`.
    pub api: String,
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
    pub tools: Arc<[Arc<dyn super::AgentTool>]>,
    /// The model being used for this turn.
    pub model: Model,
    /// Current thinking level.
    pub thinking_level: Option<String>,
    /// Prompt cache retention preference for this turn.
    pub cache_retention: CacheRetention,
    /// Session identifier forwarded to providers that support session-based
    /// caching (`prompt_cache_key`). Ignored by providers that don't.
    pub session_id: Option<String>,
    /// Per-request provider options taken from the harness turn snapshot —
    /// headers, timeout, and output budget. They overlay the stream
    /// builder's own options for this request only.
    pub stream_options: StreamOptions,
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
            .field("stream_options", &self.stream_options)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Supplies queued messages to inject into the run.
pub type MessageQueueFn = Box<dyn Fn() -> Vec<AgentMessage> + Send + Sync>;
/// The refresh a `prepare_next_turn` returns for the next turn: the model
/// and thinking level snapshot (TS `AgentLoopTurnUpdate`). The loop applies
/// it to its in-flight context before the next provider request.
#[derive(Debug, Clone)]
pub struct TurnUpdate {
    pub model: Model,
    pub thinking_level: Option<String>,
    /// Active tool subset for the next turn; `None` keeps the current set.
    pub active_tool_names: Option<Vec<String>>,
    /// Messages recorded outside the turn — a shell command the user ran —
    /// that became durable during this boundary and so must join the context
    /// the next request carries.
    pub appended_messages: Vec<AgentMessage>,
}

/// Refreshes the context/model before a turn; `None` keeps the current turn.
/// Async so the refresh can flush durable writes (TS `prepareNextTurn`);
/// takes no context reference so the future is `'static`.
pub type PrepareTurnFn = Box<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<TurnUpdate>, anyhow::Error>> + Send>,
        > + Send
        + Sync,
>;
/// Decides whether the run should stop after a turn (TS
/// `shouldStopAfterTurn`), called after `turn_end` and `prepareNextTurn` and
/// before the next LLM call. Sync like the other decision hooks
/// (`before_tool_call`/`after_tool_call`); the TS `Promise<boolean>` allowance
/// is a superset not exercised by the graceful-stop contract.
///
/// Args mirror the TS `ShouldStopAfterTurnContext` fields, in order:
/// `(message, tool_results, context, new_messages)`. Plain `&` params carry
/// implicit higher-ranked lifetimes so callers can box a closure straight
/// (a lifetime-parameterized context struct would defeat closure→`dyn` HRTB).
pub type StopAfterTurnFn = Box<
    dyn Fn(&AgentMessage, &[AgentMessage], &AgentContext, &[AgentMessage]) -> bool + Send + Sync,
>;
/// Gates a tool call before execution; `Some(reason)` blocks it.
pub type BeforeToolCallFn = Box<dyn Fn(&str, &str, &JsonValue) -> Option<String> + Send + Sync>;
/// Patches a tool result after execution.
pub type AfterToolCallFn = Box<dyn Fn(&AgentToolResult) -> AgentToolResult + Send + Sync>;
/// Observes the context right before it is sent to the provider and may
/// return a mutated copy; the returned context is what the provider sees.
pub type BeforeProviderRequestFn = Box<dyn Fn(&AgentContext) -> AgentContext + Send + Sync>;

/// Configuration for a single agent loop invocation.
#[derive(Default)]
pub struct AgentLoopConfig {
    /// Callback to get queued steering messages (injected mid-turn).
    pub get_steering_messages: Option<MessageQueueFn>,
    /// Callback to get follow-up messages (injected after turn settles).
    pub get_follow_up_messages: Option<MessageQueueFn>,
    /// Called before each turn to potentially refresh context/model.
    pub prepare_next_turn: Option<PrepareTurnFn>,
    /// Resolves the provider runtime for the current model, per turn. When
    /// set, each provider call uses the stream function for `context.model`;
    /// when `None`, the run's fixed stream fn serves every turn.
    pub stream_resolver: Option<crate::agent_loop::StreamResolver>,
    /// Called after each turn to decide whether to stop.
    pub should_stop_after_turn: Option<StopAfterTurnFn>,
    /// Called before a tool call executes. Return `Some(reason)` to block.
    pub before_tool_call: Option<BeforeToolCallFn>,
    /// Called after a tool call executes to patch the result.
    pub after_tool_call: Option<AfterToolCallFn>,
    /// Called right before the context is handed to the provider, each turn.
    pub before_provider_request: Option<BeforeProviderRequestFn>,
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamOptions {
    /// Maximum output tokens.
    pub max_tokens: Option<usize>,
    /// Temperature override.
    pub temperature: Option<f32>,
    /// Extra headers merged into every request, e.g. gateway auth.
    pub headers: Vec<(String, String)>,
    /// Per-request timeout; `None` uses the client's default.
    pub timeout: Option<std::time::Duration>,
}

impl StreamOptions {
    /// Overlay per-request options on top of the stream builder's: a field
    /// set on the request wins, unset fields fall back to the builder's, and
    /// request headers append after (and therefore override same-name)
    /// builder headers.
    pub fn overlay(&self, request: &StreamOptions) -> StreamOptions {
        let mut headers = self.headers.clone();
        headers.extend(request.headers.iter().cloned());
        StreamOptions {
            max_tokens: request.max_tokens.or(self.max_tokens),
            temperature: request.temperature.or(self.temperature),
            headers,
            timeout: request.timeout.or(self.timeout),
        }
    }
}

// ── Re-export from tool module ──────────────────────────────────────────────

use super::tool::AgentToolResult;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The TS Pi v3 on-disk shape: assistant with a `toolCall` block and a
    /// `toolResult` message, plus a usage block carrying `cacheRead`/`cost`.
    /// Round-trips through serde with camelCase field names.
    #[test]
    fn assistant_with_toolcall_and_toolresult_roundtrips_ts_pi_v3_shape() {
        let assistant = json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "tc_1",
                "name": "Read",
                "arguments": {"path": "/etc/hosts"}
            }],
            "model": "claude-opus-4-7",
            "provider": "anthropic",
            "api": "anthropic",
            "stopReason": "toolUse",
            "usage": {
                "input": 120,
                "output": 40,
                "cacheRead": 800,
                "cacheWrite": 0,
                "totalTokens": 960,
                "cost": {"input": 0.001, "output": 0.002, "cacheRead": 0.0005, "cacheWrite": 0.0, "total": 0.0035}
            },
            "timestamp": 1779952472751i64
        });
        let msg: AgentMessage = serde_json::from_value(assistant).unwrap();
        match &msg {
            AgentMessage::Assistant {
                content,
                stop_reason,
                usage,
                ..
            } => {
                assert_eq!(*stop_reason, Some(StopReason::ToolUse));
                match &content[0] {
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        assert_eq!(id, "tc_1");
                        assert_eq!(name, "Read");
                        assert_eq!(input, &json!({"path": "/etc/hosts"}));
                    }
                    other => panic!("expected ToolUse block, got {other:?}"),
                }
                assert_eq!(usage.input_tokens, 120);
                assert_eq!(usage.output_tokens, 40);
                assert_eq!(usage.cache_read_input_tokens, 800);
                assert_eq!(usage.total_tokens, 960);
                let cost = usage.cost.as_ref().expect("cost present");
                assert_eq!(cost.total, 0.0035);
            }
            other => panic!("expected Assistant, got {other:?}"),
        }

        // Re-serialize and the camelCase names survive the round trip.
        let reround = serde_json::to_value(&msg).unwrap();
        assert_eq!(reround["role"], "assistant");
        assert_eq!(reround["content"][0]["type"], "toolCall");
        assert_eq!(reround["content"][0]["arguments"]["path"], "/etc/hosts");
        assert_eq!(reround["stopReason"], "toolUse");
        assert_eq!(reround["usage"]["cacheRead"], 800);
        assert_eq!(reround["usage"]["totalTokens"], 960);
        assert_eq!(reround["usage"]["cost"]["cacheRead"], 0.0005);

        let tool_result = json!({
            "role": "toolResult",
            "toolCallId": "tc_1",
            "toolName": "Read",
            "content": [{"type": "text", "text": "127.0.0.1 localhost"}],
            "isError": false
        });
        let tr: AgentMessage = serde_json::from_value(tool_result).unwrap();
        match &tr {
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                is_error,
                ..
            } => {
                assert_eq!(tool_call_id, "tc_1");
                assert_eq!(tool_name, "Read");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        let tr_reround = serde_json::to_value(&tr).unwrap();
        assert_eq!(tr_reround["role"], "toolResult");
        assert_eq!(tr_reround["toolCallId"], "tc_1");
        assert_eq!(tr_reround["toolName"], "Read");
        assert_eq!(tr_reround["isError"], false);
    }

    #[test]
    fn bash_execution_roundtrips_ts_pi_v3_shape() {
        let wire = json!({
            "role": "bashExecution",
            "command": "cargo test",
            "output": "ok",
            "exitCode": 1,
            "cancelled": false,
            "truncated": true,
            "fullOutputPath": "/tmp/pi-bash-1.log",
            "excludeFromContext": true,
            "timestamp": 1_700_000_000_000i64
        });
        let msg: AgentMessage = serde_json::from_value(wire).unwrap();
        match &msg {
            AgentMessage::BashExecution {
                command,
                output,
                exit_code,
                cancelled,
                truncated,
                full_output_path,
                exclude_from_context,
                ..
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(output, "ok");
                assert_eq!(*exit_code, Some(1));
                assert!(!cancelled);
                assert!(truncated);
                assert_eq!(full_output_path.as_deref(), Some("/tmp/pi-bash-1.log"));
                assert_eq!(*exclude_from_context, Some(true));
            }
            other => panic!("expected BashExecution, got {other:?}"),
        }
        let reround = serde_json::to_value(&msg).unwrap();
        assert_eq!(reround["role"], "bashExecution");
        assert_eq!(reround["exitCode"], 1);
        assert_eq!(reround["fullOutputPath"], "/tmp/pi-bash-1.log");
        assert_eq!(reround["excludeFromContext"], true);
        assert_eq!(reround["timestamp"], 1_700_000_000_000i64);
    }

    #[test]
    fn bash_execution_omits_unset_optional_fields() {
        let msg = AgentMessage::BashExecution {
            command: "Ls".into(),
            output: String::new(),
            exit_code: None,
            cancelled: true,
            truncated: false,
            full_output_path: None,
            exclude_from_context: None,
            timestamp: Utc::now(),
        };
        let wire = serde_json::to_value(&msg).unwrap();
        assert!(wire.get("exitCode").is_none());
        assert!(wire.get("fullOutputPath").is_none());
        assert!(wire.get("excludeFromContext").is_none());
        assert_eq!(wire["cancelled"], true);
    }
}
