//! Where a session's journal lives.
//!
//! The single wire-id → path mint. It lives in the runtime crate because both
//! the AHP adapter and the v2 gateway resolve journals through it, and the
//! gateway is the half that goes away.

use std::path::PathBuf;

pub fn persisted_session_file(session_id: &str) -> Option<PathBuf> {
    // B5 (review round 2): wire-supplied ids reach this join BEFORE the
    // not-found guards (PageHistory / the follow cold read), so an
    // unvalidated id could probe arbitrary jsonl-shaped files under the
    // manox home ("subagents/<uuid>", "../../x"). Session ids are the
    // minters' uuid charset; admit ASCII alphanumeric, '-' and '_' only —
    // no separators, no dots, no control bytes — and keep this function
    // the sole wire-id → path mint (the repository's own
    // `session_file_name` join reads ids from journal headers, never from
    // the wire).
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // Sessions-dir single authority (review round 3, P0-1): the thread
    // store owns the sessions dir — the production store is built from
    // `paths::sessions_dir()` (same value, no behavior change), a test
    // store points at its standalone temp dir, and the gateway's cold read
    // must resolve through the store's seam or store-side fixtures starve
    // the cold path (the `sidebar_thread_switch_restores_transcript` red).
    // An uninitialized store falls back to the paths authority (the
    // pre-fix behavior).
    let dir = manox_agent::thread_store::try_global()
        .map(|_| manox_agent::thread_store::global_sessions_dir())
        .or_else(|| manox_agent::paths::sessions_dir().ok())?;
    Some(
        dir.join(manox_harness::session::repository::session_file_name(
            session_id,
        )),
    )
}
