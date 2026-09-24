//! The in-process transport: the desktop app's path to the host.
//!
//! Both halves live in one process, so messages cross as typed
//! [`JsonRpcMessage`] values — no serialization on the hot path (the v2
//! protocol reached the same conclusion: the in-process pair is unbounded and
//! must never block the GPUI main thread). The wire shape is still identical:
//! the host and the SDK client exchange the same message values they would
//! serialize over a socket, and the conformance suite runs one scenario through
//! both this transport and the WebSocket one.

use ahp::{Transport, TransportError as ClientTransportError, TransportMessage};
use ahp_types::messages::JsonRpcMessage;

use super::{HostTransport, Sink, Source, TransportError, TransportFuture};

/// A channel-backed sink, shared by the in-process and WebSocket adapters.
pub(crate) struct ChanSink {
    pub(crate) tx: async_channel::Sender<JsonRpcMessage>,
}

impl Sink for ChanSink {
    fn send(&mut self, msg: JsonRpcMessage) -> TransportFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.tx
                .send(msg)
                .await
                .map_err(|_| TransportError("connection closed".into()))
        })
    }
}

/// A channel-backed source, shared by the in-process and WebSocket adapters.
pub(crate) struct ChanSource {
    pub(crate) rx: async_channel::Receiver<JsonRpcMessage>,
}

impl Source for ChanSource {
    fn recv(&mut self) -> TransportFuture<'_, Result<Option<JsonRpcMessage>, TransportError>> {
        Box::pin(async move { Ok(self.rx.recv().await.ok()) })
    }
}

/// The client half of an in-process pair: an [`ahp::Transport`] for
/// `ahp::Client::connect`.
pub struct InprocClientTransport {
    tx: async_channel::Sender<JsonRpcMessage>,
    rx: async_channel::Receiver<JsonRpcMessage>,
}

impl Transport for InprocClientTransport {
    async fn send(&mut self, msg: TransportMessage) -> Result<(), ClientTransportError> {
        let parsed = match msg {
            TransportMessage::Parsed(msg) => msg,
            TransportMessage::Text(text) => crate::wire::parse_text(&text)
                .map_err(|err| ClientTransportError::Protocol(err.message))?,
            TransportMessage::Binary(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|err| ClientTransportError::Protocol(err.to_string()))?;
                crate::wire::parse_text(&text)
                    .map_err(|err| ClientTransportError::Protocol(err.message))?
            }
        };
        self.tx
            .send(parsed)
            .await
            .map_err(|_| ClientTransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<TransportMessage>, ClientTransportError> {
        Ok(self.rx.recv().await.ok().map(TransportMessage::Parsed))
    }
}

/// An unbounded in-process pair: `(host side, client side)`.
///
/// Unbounded is deliberate (v2 C6): two ends in one process filling a bounded
/// pair in both directions deadlocks the GPUI main thread. A slow in-process
/// client costs memory, never frames.
pub fn pair() -> (HostTransport, InprocClientTransport) {
    let (to_host_tx, to_host_rx) = async_channel::unbounded();
    let (to_client_tx, to_client_rx) = async_channel::unbounded();
    let host = HostTransport::new(
        Box::new(ChanSink { tx: to_client_tx }),
        Box::new(ChanSource { rx: to_host_rx }),
    );
    let client = InprocClientTransport {
        tx: to_host_tx,
        rx: to_client_rx,
    };
    (host, client)
}
