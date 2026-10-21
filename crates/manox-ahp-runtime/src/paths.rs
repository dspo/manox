//! Where a session's journal lives.
//!
//! The single wire-id → path mint. It lives in the runtime crate because both
//! the AHP adapter and the v2 gateway resolve journals through it, and the
//! gateway is the half that goes away.

use std::path::PathBuf;

/// The file a NEW session will write (the core layout), after a walk for an
/// already-materialized journal — create flows resolve "the path it will
/// write" through this, never the bare walk.
///
/// The shard comes from the conversation's *project*, not from a working
/// directory: belonging and cwd are disjoint, so a tool moving the workspace
/// must not move the journal, and a conversation with no project shards into
/// the one unfiled bucket rather than scattering by whatever temporary
/// directory its first turn happens to run in. Callers resolve the project
/// from the object row — for a fork, from the row of the session it copies.
pub fn session_file_under(session_id: &str, project: Option<&std::path::Path>) -> Option<PathBuf> {
    if let Some(found) = persisted_session_file(session_id) {
        return Some(found);
    }
    Some(
        manox_agent::paths::core_session_file(project, session_id).unwrap_or_else(|_| {
            manox_agent::paths::core_root()
                .unwrap_or_default()
                .join(session_id)
                .join(manox_harness::session::identity::JOURNAL_FILE)
        }),
    )
}

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
        .or_else(|| manox_agent::paths::core_root().ok())?;
    // The core root is two-deep: project buckets over per-session
    // directories, the journal (`log.v1.jsonl`) inside each — a session's
    // directory name IS its id. The walk is bounded to that shape and never
    // falls back to the legacy `sessions/` tree (dead since the cutover);
    // an id with no file anywhere resolves `None`, and the create flows
    // mint their paths through the core layout helpers, not this gate.
    fn find(dir: &std::path::Path, id: &str, depth: u8) -> Option<std::path::PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some(id) {
                    let log = path.join(manox_harness::session::identity::JOURNAL_FILE);
                    if log.is_file() {
                        return Some(log);
                    }
                    continue;
                }
                if depth > 0
                    && let Some(hit) = find(&path, id, depth - 1)
                {
                    return Some(hit);
                }
            }
        }
        None
    }
    find(&dir, session_id, 2)
}
