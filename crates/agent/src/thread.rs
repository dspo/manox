//! The `Thread` facade — the UI-facing conversation type shared by both
//! harness builds.
//!
//! One type serves both harnesses, selected at build time:
//! - `harness-manox` (default): the manox harness drives the thread directly.
//! - `harness-pi`: the pi harness backs the thread through a `ThreadEngine`
//!   (see `pi_engine`), with events drained on the gpui thread.
//!
//! The public API surface is identical across features so the workspace and
//! its views compile against one shape; the pi build adapts the shared
//! contract (`ThreadEvent`, message history, usage) rather than the reverse.

#[cfg(not(feature = "harness-pi"))]
include!("thread_manox.rs");

#[cfg(feature = "harness-pi")]
include!("thread_pi.rs");
