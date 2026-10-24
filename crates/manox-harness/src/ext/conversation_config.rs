//! The one read surface for a conversation's user-owned configuration.
//!
//! A conversation's picks — model, access mode, reasoning effort — have
//! exactly one durable home, and it is not the journal: for the desktop app
//! it is the object store's `conversations` row. Every reader goes through
//! this trait, so "the model is read here, the permission is read there"
//! cannot drift into two answers for one conversation.
//!
//! The two call points differ in *timing*, not in source:
//!
//! - the engine's request boundary takes a snapshot (`model`,
//!   `reasoning_effort`) and compares `config_version` against the version it
//!   last applied, so a mid-conversation switch is applied once and
//!   announced to the model ahead of the turn that triggered it;
//! - the access gate reads `access_mode` again before **every tool call**,
//!   so a switch governs the very next tool rather than the next turn.
//!
//! The host installs one implementation per conversation. Implementations
//! must be cheap and side-effect free: `access_mode` is read per tool call,
//! not once per turn.

use crate::core::types::Model;
use crate::ext::sandbox::AccessMode;

/// A conversation's configuration picks, read from wherever they durably
/// live. `None` from any getter means "this conversation has no pick", never
/// "the read failed" — the caller keeps the value it already holds.
pub trait ConversationConfigSource: Send + Sync {
    /// The picked model, resolved against the live provider registry.
    ///
    /// `None` while the conversation has no pick — and also while the pick
    /// cannot be resolved yet (a provider that is not registered). The pair
    /// with [`Self::model_ref`] is what tells the two apart, which matters
    /// to a reader that advances a watermark on success: an unresolvable
    /// pick must stay pending so a later boundary retries it.
    fn model(&self) -> Option<Model>;

    /// The model pick exactly as stored (`provider/id`, or a bare id when
    /// the conversation carries no provider), independent of whether it
    /// resolves. `None` when the conversation has no model pick at all.
    fn model_ref(&self) -> Option<String>;

    /// The picked access mode.
    fn access_mode(&self) -> Option<AccessMode>;

    /// The picked reasoning effort, in the wire vocabulary the engine's
    /// thinking level uses (`"high"` / `"max"`).
    fn reasoning_effort(&self) -> Option<String>;

    /// Bumped by every configuration write. A reader that compares it
    /// against the version it last applied can skip a snapshot entirely when
    /// nothing was written, and can tell a real change from a re-statement
    /// of the same picks.
    fn config_version(&self) -> i64;
}
