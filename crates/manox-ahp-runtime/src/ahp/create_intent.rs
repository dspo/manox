//! The in-memory create intent of a session this process just accepted.
//!
//! A deferred create writes nothing: the engine buffers its create facts in
//! memory until the first turn materializes the journal, and a client without
//! a conversation row has no durable place for the picks either. The seed has
//! to answer with the intent anyway — a client that subscribes right after
//! `createSession` must see the values it asked for, not the defaults.
//!
//! This map is that record, keyed by session id and held only while the
//! process drives the session. It is deliberately not an authority: the
//! desktop's object row is durable draft intent, the journal overrides both
//! once it has rows, and a process restart loses this map without weakening
//! any of those ([`super::session_state`] states the order).

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

/// The config and working-directory intent one accepted `createSession`
/// carried.
#[derive(Debug, Clone, Default)]
pub(crate) struct Intent {
    pub(crate) project: Option<String>,
    /// Canonical `provider/model` reference.
    pub(crate) model: Option<String>,
    /// Access-mode wire string (`sandbox` / `full-access`).
    pub(crate) access_mode: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    /// Working directories as plain paths (the create's `file://` URIs
    /// decoded).
    pub(crate) working_directories: Vec<String>,
}

fn intents() -> &'static Mutex<HashMap<String, Intent>> {
    static INTENTS: OnceLock<Mutex<HashMap<String, Intent>>> = OnceLock::new();
    INTENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record what a create accepted for `session_id`.
pub(crate) fn note(session_id: &str, intent: Intent) {
    intents().lock().insert(session_id.to_string(), intent);
}

/// The create intent recorded for `session_id` in this process, if any.
pub(crate) fn get(session_id: &str) -> Option<Intent> {
    intents().lock().get(session_id).cloned()
}

/// Drop the record for a disposed session: nothing in this process will
/// subscribe to it again. The object row, when one exists, stays the
/// desktop's durable intent and is read afresh by the next seed.
pub(crate) fn forget(session_id: &str) {
    intents().lock().remove(session_id);
}
