//! The `Thread` facade — the UI-facing conversation type.
//!
//! The pi harness backs the thread through a `ThreadEngine` (see
//! `pi_engine`), with events drained on the gpui thread. The retired manox
//! harness implementation was removed; see git history (or the
//! `origin/Manox` backup branch) for it.

include!("thread_pi.rs");
