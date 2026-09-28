//! JSON-RPC 2.0 mechanics: correlation, encoding, version negotiation.
//!
//! This layer knows nothing about AHP — no channels, no state, no actions. It is
//! what `core/` builds on, and it is where the v2 protocol's most expensive
//! lessons were paid for:

//! - [`peer::RpcPeer`] correlates outstanding request/response and call/reply
//!   pairs. Its three hard rules are carried over verbatim in behaviour:
//!   a duplicate registration is refused and the **first** waiter survives; a
//!   closed waiter must never be produced by our own bookkeeping (callers fold a
//!   closed receiver into a fail-closed rejection, so a clobbered waiter
//!   auto-denies an approval before the user answers); and a same-client
//!   hand-shake cancels every outstanding waiter with a distinguishable error so
//!   senders can tell a connection swap from a delivery failure.
//! - Timeouts are deliberately **not** here. The issuer owns its own deadline
//!   (the v2 gateway ran a 300s call timeout), because a peer that guesses a
//!   timeout for its caller cannot let a longer one through.
//! - [`version::negotiate`] is the single place that maps a client's offered
//!   protocol versions onto the one this build speaks.

pub mod peer;
pub mod version;

pub use peer::RpcPeer;

/// Correlation id of an outstanding request (client- or server-initiated).
///
/// AHP's wire shape carries a `u64` id, so this is a newtype over it rather than
/// a string: the generated `ahp_types::messages::JsonRpcRequest` cannot hold
/// anything else, and a client that sends a string id fails at parse time with a
/// clear parse error instead of a correlation miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MsgId(pub u64);

impl MsgId {
    /// A correlator minting ids from `next`: the caller owns the counter, so a
    /// connection's ids are dense and traceable.
    pub fn next(next: &std::sync::atomic::AtomicU64) -> Self {
        Self(next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
    }
}

impl std::fmt::Display for MsgId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
