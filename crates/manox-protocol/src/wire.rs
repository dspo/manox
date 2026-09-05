//! List-channel wire types shared by the registry notifications and host
//! events (`ThreadsUpdated` / `Models`).
//!
//! T10 (§D.6): the `WireMessage` transcript cluster existed only to feed the
//! doomed `ServerNote::ThreadHistory` arm; the durable transcript now travels
//! as `message` journal entries (`JournalWireEvent::Message`) and the
//! `PageHistory` / snapshot `records`. Field names are snake_case on the
//! wire to match the existing client contract.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

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
