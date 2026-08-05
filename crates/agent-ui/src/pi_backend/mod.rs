//! The pi harness backend (built only with `feature = "harness-pi"`).
//!
//! Lets the existing, polished workspace drive a pi `AgentSession` instead of
//! the manox `Thread`: [`session::PiSession`] quacks like the thread the
//! workspace already holds (same `ThreadEvent` stream, same `agent::Message`
//! history), and [`adapt`] holds the pure mappings between the two worlds.
//! manox adapts to `crates/{pi, pi-extensions}` — never the other way round.

pub mod adapt;
pub mod session;

pub use session::PiSession;
