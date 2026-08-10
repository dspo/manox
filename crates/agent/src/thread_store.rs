// The `ThreadStore` facade — the session-list state the sidebar renders.
//
// The pi build lists the pi session repository plus a per-session
// UI-metadata sidecar. The retired manox SQLite-backed implementation was
// removed; see git history (or the `origin/Manox` backup branch) for it.

include!("thread_store_pi.rs");
