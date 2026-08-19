//! UI annotation card types (Error / Notice / PlanReview).
//!
//! These cards are host-only UI state that never enters the model-facing
//! canonical `Thread::messages` (so `build_completion_request` — and the
//! provider prompt-cache prefix — is unaffected). They persist as `custom`
//! entries in the session jsonl tree (`custom_type = UI_NOTE_CUSTOM_TYPE`,
//! payload = the serialized `UiNoteRecord`), appended at emit time so the
//! append order — and therefore every reload — reproduces their
//! chronological position among the messages.

use serde::{Deserialize, Serialize};

/// The `custom_type` tag of session entries carrying a [`UiNoteRecord`].
pub const UI_NOTE_CUSTOM_TYPE: &str = "manox_ui_note";

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
}

/// One UI annotation card. `data` carries the render payload — currently
/// `{ "text": String }`, plus `tool_call_id` for approval decision records —
/// and is left as raw JSON so future note shapes extend without a schema
/// change. Entry append order in the session jsonl equals emit order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiNoteRecord {
    pub kind: UiNoteKind,
    pub data: serde_json::Value,
}

/// A note plus its position in the session entry sequence: the count of
/// message entries preceding it. Stable across reloads (derived from the
/// tree) and across mid-run live mirror refreshes (re-merged by this count).
#[derive(Debug, Clone)]
pub struct PositionedNote {
    pub note: UiNoteRecord,
    pub after_message: usize,
}

/// One element of the display sequence the engine mirror and the
/// conversation rebuild share: messages interleaved with the UI annotation
/// cards that landed between them.
#[derive(Debug, Clone)]
pub enum HistoryEntry {
    Message(crate::message::Message),
    Note(UiNoteRecord),
}
