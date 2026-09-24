//! The AHP host's runtime half.
//!
//! [`manox_ahp`] is the protocol: JSON-RPC, channels, the state store, the
//! action pipeline. It knows nothing about manox — its `Backend` trait is the
//! only window, and a test doubles it with a scripted backend. This crate is
//! that trait's real implementor, plus the shapes the protocol asks a runtime
//! for.
//!
//! The split matters in one direction. `manox-ahp` must not depend on this crate
//! (a transport that knows the runtime cannot be exercised against a fake one),
//! and `manox-agent` must not depend on either (the kernel does not know AHP
//! exists — capabilities reach it through its own hooks).

pub mod error;
pub mod runtime_trait;
