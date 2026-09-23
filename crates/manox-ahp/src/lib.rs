//! manox-ahp — the manox side of the [Agent Host Protocol] (AHP).
//!
//! This crate is the **host half** of AHP: AHP ships client SDKs only, so the
//! JSON-RPC router, the global `serverSeq` sequencer, channel registration and
//! subscriptions, snapshot/reconnect servicing, the action acceptance table and
//! the `resource*` file plane live here. Frontends (the GPUI desktop app in
//! `dspo/manox-app`, remote AHP clients, VS Code's agent window) connect to a
//! [`Host`] over any AHP transport; the desktop uses the in-process transport,
//! everything else the `/ahp` WebSocket route.
//!
//! Layering (architecture doc `docs/ahp-v3-architecture.md`):
//!
//! ```text
//! manox-journal (durable vocabulary)  ->  translate/ (entries -> AHP actions)
//! manox-session-core (implements Backend: real sessions, side effects)
//! manox-ahp (this crate: protocol, channels, sequencer, transports)
//! ```
//!
//! Invariants kept from the v2 protocol are listed in the architecture doc §C.
//! Two of them shape this crate:
//!
//! - **The journal stays the only durable store.** Channel state here is a fold;
//!   a fresh [`Host`] seeds it through [`Backend`] and then advances it with the
//!   same action stream clients receive, so host and client reductions cannot
//!   drift (the convergence gate in `tests/`).
//! - **One gateway per process.** A single [`Host`] owns the single `serverSeq`
//!   domain; connections multiplex onto it.
//!
//! [Agent Host Protocol]: https://microsoft.github.io/agent-host-protocol/

pub mod backend;
pub mod channels;
pub mod connection;
pub mod error;
pub mod ext;
pub mod host;
pub mod jsonrpc;
pub mod resource;
pub mod router;
pub mod sequencer;
pub mod translate;
pub mod transport;
pub mod wire;

pub use ahp_types::actions::{ActionEnvelope, ActionOrigin, StateAction};
pub use ahp_types::common::{AnyValue, Uri};
pub use backend::{Backend, DispatchOutcome};
pub use error::{HostError, codes};
pub use host::Host;
pub use transport::HostTransport;
