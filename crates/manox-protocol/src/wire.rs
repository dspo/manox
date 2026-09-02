//! Wire form of a restored conversation message — the schema for
//! [`crate::ServerNote::ThreadHistory`] `messages`. Mirrors the storage
//! [`manox_agent::message::Message`] shape except that inline image bytes are
//! deflated to a `byte_len` placeholder (bounded wire payload); the client
//! store reconstructs transcript items from these without ever needing the
//! raw bytes. Defined here (not in `manox-agent`) so the protocol crate stays
//! the single source of client-facing types and never depends on the agent
//! crate; the server performs the `Message → WireMessage` projection.
//!
//! Field names are snake_case on the wire (no `rename_all`) to match the
//! storage serde shape and the existing client expectations.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Wire form of [`manox_agent::message::Message`]. Content blocks are
/// externally tagged; image blocks arrive deflated to `{ mime_type, byte_len }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct WireMessage {
    pub id: String,
    /// Unix seconds.
    pub timestamp: i32,
    pub parent_id: Option<String>,
    pub provenance: WireMessageProvenance,
    pub role: WireRole,
    pub content: Vec<WireContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ui: Option<WireMessageUi>,
}

/// Stable origin of a persisted message. Mirrors
/// [`manox_agent::message::MessageProvenance`] (snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "snake_case")]
pub enum WireMessageProvenance {
    User,
    Assistant,
    Tool,
}

/// Conversation role. Mirrors [`manox_agent::language_model::Role`]
/// (snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "snake_case")]
pub enum WireRole {
    User,
    Assistant,
    System,
}

/// One content block of a [`WireMessage`]. Externally tagged (serde default):
/// `{"Text": "..."}` | `{"Thinking": {"text","signature"}}` |
/// `{"Image": {"mime_type","byte_len"}}` | `{"ToolUse": {...}}` |
/// `{"ToolResult": {...}}` | `{"Compaction": "..."}`. The `Image` payload is
/// the deflated wire shape — `byte_len` replaces the base64 `data` the storage
/// type carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub enum WireContentBlock {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    Image {
        mime_type: String,
        byte_len: u32,
    },
    ToolUse(WireToolUse),
    ToolResult(WireToolResult),
    Compaction(String),
}

/// Wire form of [`manox_agent::language_model::LanguageModelToolUse`]. `name`
/// is a plain `String` on the wire (storage uses `Arc<str>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct WireToolUse {
    pub id: String,
    pub name: String,
    pub raw_input: String,
    pub input: serde_json::Value,
    pub is_input_complete: bool,
    pub thought_signature: Option<String>,
}

/// Wire form of [`manox_agent::language_model::LanguageModelToolResult`].
/// `tool_name` is a plain `String` on the wire (storage uses `Arc<str>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct WireToolResult {
    pub tool_use_id: String,
    pub tool_name: String,
    pub is_error: bool,
    pub content: String,
}

/// Wire form of [`manox_agent::message::MessageUiMetadata`]. All fields are
/// optional — omitted from the wire when absent/false so the client treats
/// every key as optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct WireMessageUi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub model_id: Option<String>,
    /// `PermissionMode::as_i64`, stored as an integer to avoid coupling the
    /// message schema to enum names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub approval_mode: Option<i32>,
    /// Set when this user message was injected mid-turn via the steer queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub steered: Option<bool>,
    /// Machine-generated background-task event; the UI must not attribute it to
    /// the human user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub external_event: Option<bool>,
    /// Originating agent of a user-role message the human did not type;
    /// absent = human input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub author: Option<WireMessageAuthor>,
    /// This user message entered the session via team peer delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub peer: Option<bool>,
    /// UI-only display form (e.g. the compact `/name args` for a slash turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub display_text: Option<String>,
}

/// Wire form of [`manox_agent::message::MessageAuthor`] (snake_case,
/// externally tagged: `"lead"` | `"harness"` | `{"agent": "..."}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "snake_case")]
pub enum WireMessageAuthor {
    #[default]
    Lead,
    Harness,
    Agent(String),
}

/// One row in the threads list — the wire schema for
/// [`crate::ServerNote::ThreadsUpdated`] `threads`. Combines the persisted
/// [`manox_agent::db::ThreadSummary`] columns with the live runtime flags
/// (`running` / `pending_auth` / `pending_plan` / `background_work`) the
/// thread store tracks; the server projects both into this flat shape so the
/// client list never reads two sources. Field names are snake_case on the
/// wire to match the existing client contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct ThreadListItem {
    pub id: String,
    pub title: String,
    /// Unix seconds of the last interaction.
    pub updated_at: i32,
    pub running: bool,
    pub unread: bool,
    pub errored: bool,
    pub pending_auth: bool,
    /// A plan review verdict is due; the row shows the static blue wheel.
    pub pending_plan: bool,
    /// Live monitors / background bash keep the loop self-advancing; the row
    /// spins even with no turn in flight.
    pub background_work: bool,
    pub model_id: String,
    pub pinned: bool,
    pub archived: bool,
    /// Leader session id for a team worker row; null for a top-level row.
    pub parent_id: Option<String>,
    /// Nesting level: 0 is top-level, 1 is a team member of a top-level
    /// leader, and so on.
    pub depth: i32,
}

/// One selectable model in the models list — the wire schema for
/// [`crate::ServerNote::Models`] `models`. Field names are snake_case on the
/// wire to match the existing client contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    /// Provider display name (e.g. "DeepSeek" for the "DeepSeek-anthropic"
    /// registration id). Absent only from older actors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub provider_name: Option<String>,
    /// Wire API shape ("anthropic", "openai_responses", …); drives the
    /// cascade menu's badge and tint.
    pub api: String,
    pub context_window: u32,
    /// Per-model output budget; absent from older actors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_tokens: Option<u32>,
}
