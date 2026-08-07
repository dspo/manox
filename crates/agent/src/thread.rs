//! The `Thread` facade — the UI-facing conversation type.
//!
//! The pi harness backs the thread through a `ThreadEngine` (see
//! `pi_engine`), with events drained on the gpui thread. The retired manox
//! harness implementation is archived in the `harness-manox` crate.

include!("thread_pi.rs");
