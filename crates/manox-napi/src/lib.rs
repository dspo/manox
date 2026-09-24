//! napi-rs bindings exposing the manox agent core to a TypeScript host.
//!
//! The binding is a **raw transport bridge**, nothing more: it hands the
//! TypeScript side one end of an in-process AHP connection and moves complete
//! JSON-RPC messages across it in both directions. The protocol itself —
//! channels, subscriptions, request correlation — belongs to the AHP client
//! SDK on the TypeScript side, and the host end is served by the
//! process-singleton AHP runtime on this side.
//!
//! Nothing here parses or interprets a message. A method or action this build
//! does not serve is refused by the host with the protocol's own error, and a
//! frame vocabulary that grows later needs no change in this file.

#[macro_use]
extern crate napi_derive;

use std::sync::Mutex;

use ahp_types::messages::JsonRpcMessage;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

/// The live connection to the in-process AHP host.
static CONN: Mutex<Option<ConnectionState>> = Mutex::new(None);

struct ConnectionState {
    /// Outbound messages, as protocol text, for the host-side sink task.
    outgoing: async_channel::Sender<String>,
    /// The pump thread handle. Dropped on shutdown.
    _pump: std::thread::JoinHandle<()>,
    /// Keeps the process-singleton runtime (and therefore the session store)
    /// alive for as long as the binding is started.
    _runtime: std::sync::Arc<manox_ahp_runtime::ahp::runtime::AhpRuntime>,
}

fn conn_slot() -> std::sync::MutexGuard<'static, Option<ConnectionState>> {
    CONN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Smoke-test export: verifies the native module loads and links the agent
/// dependency graph.
#[napi]
pub fn ping() -> String {
    "pong".to_string()
}

/// Start the agent runtime and connect one in-process AHP transport.
///
/// `event_cb` receives one JSON string per host→client message, on the Node
/// main thread, scheduled from the pump thread through a threadsafe function.
/// The caller is responsible for the protocol: it sends `initialize` and every
/// subsequent request or notification through [`send_command`].
///
/// `client_id` names the host identity to pin. The AHP handshake carries the
/// client id on each request, so this only seeds the agent host's own identity
/// (the VS Code host vs. the desktop one) — it is not a protocol-level login.
#[napi]
pub fn start(client_id: String, event_cb: JsFunction) -> Result<()> {
    // The built-in Chrome engine (rustwright-core, via ChromeUse) is NOT
    // linked into this host: the agent is built without the `chrome-use`
    // feature on this edge, so nothing here can launch it. DISABLE_TELEMETRY
    // is set as defence in depth in case the engine is ever enabled.
    unsafe { std::env::set_var("DISABLE_TELEMETRY", "1") };
    let mut slot = conn_slot();
    if slot.is_some() {
        return Err(napi::Error::from_reason("actor already started"));
    }

    // Pin the host identity and initialize the agent runtime.
    let _ = client_id;
    manox_agent::host::set_host(manox_agent::host::Host::Vscode);
    manox_agent::init();

    let tsfn: ThreadsafeFunction<String> =
        event_cb.create_threadsafe_function(0, |ctx| Ok(vec![ctx.value]))?;

    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
        .to_string_lossy()
        .to_string();

    // The gateway OWNS the session store, so it is constructed first: it
    // installs the runtime builder the process-singleton host is built from,
    // and registers the capability + embedder-tool providers the engine
    // consults when it assembles a session's tools.
    let _server = manox_session_core::agent_server::global(std::path::PathBuf::from(&cwd));
    let runtime = manox_ahp_runtime::ahp::runtime::runtime(std::path::PathBuf::from(&cwd));

    // One in-process pair. Both queues are unbounded: a bounded pair filled in
    // both directions would deadlock the Node main thread against the host.
    let (to_host_tx, to_host_rx) = async_channel::unbounded::<String>();
    let (to_client_tx, to_client_rx) = async_channel::unbounded::<String>();

    let host = manox_ahp::transport::HostTransport::new(
        Box::new(TextSink { tx: to_client_tx }),
        Box::new(TextSource { rx: to_host_rx }),
    );
    runtime.accept(host);

    let pump = std::thread::Builder::new()
        .name("manox-napi-pump".into())
        .spawn(move || {
            while let Ok(message) = to_client_rx.recv_blocking() {
                if !matches!(
                    tsfn.call(Ok(message), ThreadsafeFunctionCallMode::NonBlocking),
                    napi::Status::Ok
                ) {
                    break;
                }
            }
        })
        .map_err(|e| napi::Error::from_reason(format!("failed to spawn pump thread: {e}")))?;

    *slot = Some(ConnectionState {
        outgoing: to_host_tx,
        _pump: pump,
        _runtime: runtime,
    });
    Ok(())
}

/// Deliver one serialized AHP request or notification to the host.
#[napi]
pub fn send_command(command: String) -> Result<()> {
    match conn_slot().as_ref() {
        Some(state) => state
            .outgoing
            .try_send(command)
            .map_err(|e| napi::Error::from_reason(format!("host connection closed: {e}"))),
        None => Err(napi::Error::from_reason(
            "actor not started; call start(callback) first",
        )),
    }
}

/// Tear down the agent connection and release the event callback. Safe to call
/// when the actor is not started; intended for host `deactivate`.
#[napi]
pub fn shutdown() {
    if let Some(state) = conn_slot().take() {
        // Closing the queue ends the host's source half, which ends the
        // connection; the pump thread then sees its own queue close.
        state.outgoing.close();
    }
}

/// The host's writing half: protocol text out to the TypeScript side.
struct TextSink {
    tx: async_channel::Sender<String>,
}

impl manox_ahp::transport::Sink for TextSink {
    fn send(
        &mut self,
        msg: JsonRpcMessage,
    ) -> manox_ahp::transport::TransportFuture<'_, std::result::Result<(), manox_ahp::transport::TransportError>>
    {
        Box::pin(async move {
            let text = manox_ahp::wire::to_text(&msg);
            self.tx
                .send(text)
                .await
                .map_err(|_| manox_ahp::transport::TransportError("client closed".into()))
        })
    }
}

/// The host's reading half: protocol text in from the TypeScript side.
struct TextSource {
    rx: async_channel::Receiver<String>,
}

impl manox_ahp::transport::Source for TextSource {
    fn recv(
        &mut self,
    ) -> manox_ahp::transport::TransportFuture<
        '_,
        std::result::Result<Option<JsonRpcMessage>, manox_ahp::transport::TransportError>,
    > {
        Box::pin(async move {
            let Some(text) = self.rx.recv().await.ok() else {
                return Ok(None);
            };
            manox_ahp::wire::parse_text(&text)
                .map(Some)
                .map_err(|err| manox_ahp::transport::TransportError(err.message))
        })
    }
}
