//! Plain data types the thread store and engine share.
//!
//! The v1.0.0 Session Core retired the SQLite metadata layer: every durable
//! fact lives in the session journal (the ui-note cards are `custom` rows,
//! `UI_NOTE_CUSTOM_TYPE`), the sidebar cache rebuilds from headers, and the
//! project registry derives from the scan. What survives here are the
//! in-memory shapes those consumers still share — no database behind them.

use serde::{Deserialize, Serialize};

/// Summary used by the sidebar list (no message bodies).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadSummary {
    pub id: String,
    /// Mechanical first-user-message summary (fallback display text).
    pub summary: String,
    /// LLM-generated title, if one has been streamed.
    pub title: Option<String>,
    /// User-supplied rename; takes display precedence over `title`.
    pub title_override: Option<String>,
    pub model_id: String,
    pub provider_id: Option<String>,
    /// Access mode (`AccessMode::as_i64` mapping: 1 = sandbox, 2 =
    /// full access); sidebar wash color.
    pub access_mode: i64,
    pub project: String,
    pub depth: i32,
    pub parent_id: Option<String>,
    pub archived: bool,
    /// Pinned flag toggled from the title bar menu.
    pub pinned: bool,
    /// User-assigned tag shown as a chip on the sidebar row.
    pub tag: Option<String>,
    /// Unix seconds of the last turn that settled in this session. Never
    /// written by a viewer; the merge takes the max with what this process
    /// already holds.
    pub last_settled_at: Option<i64>,
    /// Unix seconds of the last observed view. Merge takes the max.
    pub last_viewed_at: Option<i64>,
    /// Derived attention bit: a turn settled after the last observed view
    /// ([`derive_unread`]). Never write it directly — recompute from the
    /// two inputs.
    pub has_unread: bool,
    /// Error flag: the last turn ended in a fatal error.
    pub errored: bool,
    pub created_at: i64,
    pub interacted_at: i64,
    pub updated_at: i64,
    /// Sum of the four cumulative token columns.
    pub cumulative_total_tokens: u64,
}

impl ThreadSummary {
    /// Display title with precedence: user rename > LLM title > summary.
    pub fn display_title(&self) -> &str {
        if let Some(title) = self
            .title_override
            .as_deref()
            .filter(|title| !title.is_empty())
            .or(self.title.as_deref().filter(|title| !title.is_empty()))
        {
            return title;
        }
        if !self.summary.is_empty() {
            return &self.summary;
        }
        &self.project
    }

    /// Re-derive the two monotone inputs into the attention bit, folding the
    /// stamps this process may hold beyond its last scan (`pending_viewed`).
    pub fn derive_has_unread(&self, pending_viewed: Option<i64>) -> bool {
        derive_unread(self.last_settled_at, self.last_viewed_at.max(pending_viewed))
    }

    /// Fold `other`'s monotone stamps into `self` (take the max each), then
    /// re-derive the attention bit. Returns whether the bit flipped.
    pub fn merge_stamps(&mut self, settled: Option<i64>, viewed: Option<i64>) -> bool {
        if let Some(at) = settled {
            self.last_settled_at = Some(self.last_settled_at.map_or(at, |prev| prev.max(at)));
        }
        if let Some(at) = viewed {
            self.last_viewed_at = Some(self.last_viewed_at.map_or(at, |prev| prev.max(at)));
        }
        let was = self.has_unread;
        self.has_unread = derive_unread(self.last_settled_at, self.last_viewed_at);
        self.has_unread != was
    }
}

/// The one read rule: a session is unread when a turn settled after the last
/// time it was viewed. Both inputs are monotone wall-clock stamps; a missing
/// view stamp means "never looked", so any settled turn reads unread; a
/// missing settle stamp means "nothing settled since tracking began" (the
/// upgrade path), which reads clear. Strict comparison, second precision —
/// a settle and a view in the same second read as viewed (conservative).
pub fn derive_unread(settled: Option<i64>, viewed: Option<i64>) -> bool {
    settled.is_some_and(|at| at > viewed.unwrap_or(i64::MIN))
}

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
}

/// One UI annotation card. `data` carries the render payload — currently
/// `{ "text": String }` — and is left as raw JSON so future note shapes
/// extend without a schema change.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HistoryEntry {
    Message(crate::message::Message),
    Note(UiNoteRecord),
}

/// One cached session file's bounded facts (the sidebar scan's
/// accelerator — disposable, rebuilt from headers, never a source of
/// truth).
#[derive(Debug, Clone)]
pub struct SessionIndexRow {
    pub path: std::path::PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub session_id: String,
    pub cwd: String,
    pub created_at_ns: i64,
    pub parent_session: Option<String>,
    pub metadata_json: Option<String>,
    pub first_user_text: String,
    pub has_messages: bool,
    pub failed: bool,
    pub sidecar_size: u64,
    pub sidecar_mtime_ns: i64,
    pub sidecar_json: Option<String>,
}
