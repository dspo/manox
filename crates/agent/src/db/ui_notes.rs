//! UI annotation card types (Error / Notice / PlanReview / AutoApproval).
//!
//! These cards are host-only UI state that never enters the model-facing
//! canonical `Thread::messages` (so `build_completion_request` — and the
//! provider prompt-cache prefix — is unaffected). They are persisted in the
//! per-session sidecar (see `SessionMeta::ui_notes`) and restored on session
//! reopen; the `Thread` facade carries the in-memory `Vec<UiNoteRecord>` and
//! `Workspace` snapshots it to the sidecar via `save_thread`.

use serde::{Deserialize, Serialize};

/// Persisted UI note kind. The string value is the stable wire identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiNoteKind {
    /// A terminal runtime error from the agent (red danger styling).
    Error,
    /// A neutral system notice — slash-command acks, mode-change chips, etc.
    Notice,
    /// A plan the user dismissed without implementing — a free-form message
    /// superseded it. Renders as a collapsed read-only `PlanReview` record so
    /// the dismissed plan survives a thread switch / reload: the live card is
    /// UI-only and never enters `Thread::messages`, so without this note it
    /// would vanish the moment the conversation entity is rebuilt.
    PlanReview,
    /// An autopilot auto-approval marker: the safety reviewer allowed a gated
    /// tool call without escalating to the user. `data` carries only
    /// `tool_call_id`; the rebuild stamps the matching tool item's badge
    /// (`ToolCallItem::auto_approved`) — this kind never renders as a
    /// conversation item.
    AutoApproval,
}

/// One UI annotation card. `data` carries the render payload — currently
/// `{ "text": String }`, plus `tool_call_id` for approval decision records —
/// and is left as raw JSON so future note shapes extend without a schema
/// change. Array order (in memory / in the sidecar) equals emit order.
///
/// The struct doubles as the sidecar wire shape (`SessionMeta::ui_notes`),
/// so it carries no SQLite-row artifacts (id / seq / ts / thread_id): the
/// per-thread in-memory Vec and the per-session sidecar both provide order
/// and ownership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiNoteRecord {
    /// User message id whose turn this note belongs to; `None` for notes
    /// emitted before any user message (placed at the top on rebuild).
    pub anchor_user_id: Option<String>,
    pub kind: UiNoteKind,
    pub data: serde_json::Value,
}
