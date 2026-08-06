// The `ThreadStore` facade — the session-list state the sidebar renders.
//
// One type serves both harness builds (see `thread.rs` for the same pattern):
// the manox build persists to SQLite, the pi build lists the pi session
// repository plus a per-session UI-metadata sidecar. The public surface is
// identical so the sidebar compiles against one shape.

#[cfg(not(feature = "harness-pi"))]
include!("thread_store_manox.rs");

#[cfg(feature = "harness-pi")]
include!("thread_store_pi.rs");
