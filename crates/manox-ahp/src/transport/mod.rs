//! Host-side transports.
//!
//! AHP does not prescribe a transport: any ordered, reliable, bidirectional
//! stream of complete messages will do, and the protocol is chosen out of band.
//! The host talks to a transport through two owned halves — a [`Sink`] and a
//! [`Source`] — so the reading and writing tasks borrow nothing shared. Two
//! implementations ship here: the in-process pair the desktop app uses (typed
//! messages, no serialization) and the WebSocket adapter behind the `axum-ws`
//! feature that the `/ahp` route mounts.

#[cfg(feature = "axum-ws")]
pub mod axum_ws;
pub mod inproc;

use std::future::Future;
use std::pin::Pin;

use ahp_types::messages::JsonRpcMessage;

/// Boxed future used by the (object-safe) sink/source traits.
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A transport failure. Fatal for the connection: the client reconnects and
/// the host answers its `reconnect` with fresh snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

/// The writing half of a connection.
pub trait Sink: Send + 'static {
    /// Send one complete message.
    fn send(&mut self, msg: JsonRpcMessage) -> TransportFuture<'_, Result<(), TransportError>>;
}

/// The reading half of a connection.
pub trait Source: Send + 'static {
    /// Next inbound message; `None` means the peer closed cleanly.
    fn recv(&mut self) -> TransportFuture<'_, Result<Option<JsonRpcMessage>, TransportError>>;
}

/// One accepted connection, as handed to [`crate::Host::accept`].
pub struct HostTransport {
    sink: Box<dyn Sink>,
    source: Box<dyn Source>,
}

impl HostTransport {
    /// Compose a transport from its halves.
    pub fn new(sink: Box<dyn Sink>, source: Box<dyn Source>) -> Self {
        Self { sink, source }
    }

    /// Take the halves apart (the host runs one task per half).
    pub fn split(self) -> (Box<dyn Sink>, Box<dyn Source>) {
        (self.sink, self.source)
    }
}

impl std::fmt::Debug for HostTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostTransport")
    }
}
