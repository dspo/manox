// The `ThreadStore` facade — the session-list state the sidebar renders.
//
// The pi build lists the pi session repository plus a per-session
// UI-metadata sidecar. The retired manox SQLite-backed implementation is
// archived in the `harness-manox` crate.

include!("thread_store_pi.rs");
