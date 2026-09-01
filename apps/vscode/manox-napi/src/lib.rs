//! napi-rs bindings exposing the manox agent core to a TypeScript host.
//!
//! Thin glue over `AgentServer` + `InProcessConnection`: the host starts the
//! agent runtime, opens an in-process protocol connection, and streams typed
//! `FromServer` messages (serialized as JSON strings) back to Node through a
//! threadsafe function. `shutdown()` disconnects the transport so window
//! reloads and `deactivate` re-initialize cleanly.

#[macro_use]
extern crate napi_derive;

use std::sync::Mutex;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use manox_protocol::handshake::{HookKind, Initialize};
use manox_protocol::msg::{FromClient, MsgId};
use manox_protocol::transport::{InProcessConnection, RpcConnection, in_process_pair};
use manox_session_core::agent_server::AgentServer;

/// The "client" end of the in-process connection. The AgentServer holds the
/// server end; the napi side sends commands to the server and receives events
/// from the server through this connection.
static CONN: Mutex<Option<ConnectionState>> = Mutex::new(None);

struct ConnectionState {
    conn: InProcessConnection,
    /// The pump thread handle. Dropped on shutdown.
    _pump: std::thread::JoinHandle<()>,
    /// The server handle. Dropped on shutdown (the server's tasks settle when
    /// the connection is disconnected).
    _server: AgentServer,
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

/// Start the agent runtime and open an in-process protocol connection to the
/// AgentServer. `event_cb` receives one serialized `FromServer` JSON string
/// per call; it runs on the Node main thread, scheduled by the pump thread
/// through a threadsafe function.
///
/// The handshake (`FromClient::Request(Initialize{...})`) is sent automatically
/// so the TS side never needs to send it.
#[napi]
pub fn start(event_cb: JsFunction) -> Result<()> {
    // The built-in Chrome engine (rustwright-core, via ChromeUse) is NOT
    // linked into the VS Code host: the agent is built without the
    // `chrome-use` feature on the manox-napi edge, so nothing here can ever
    // launch it. DISABLE_TELEMETRY is still set as defense in depth in case
    // the engine is ever enabled on this host.
    unsafe { std::env::set_var("DISABLE_TELEMETRY", "1") };
    let mut slot = conn_slot();
    if slot.is_some() {
        return Err(napi::Error::from_reason("actor already started"));
    }

    // Pin the host identity and initialize the agent runtime.
    manox_agent::host::set_host(manox_agent::host::Host::Vscode);
    manox_agent::init();

    let tsfn: ThreadsafeFunction<String> =
        event_cb.create_threadsafe_function(0, |ctx| Ok(vec![ctx.value]))?;

    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
        .to_string_lossy()
        .to_string();

    // Create the in-process connection pair and the AgentServer.
    let (client_conn, server_conn) = in_process_pair();
    let server = AgentServer::new(std::path::PathBuf::from(&cwd));

    // Accept the server-side connection (spawns the async handler on the agent
    // tokio runtime).
    server.accept(std::sync::Arc::new(server_conn));

    // Spawn the pump thread: reads `FromServer` messages from the client end
    // of the connection and pushes them as JSON strings to the Node callback.
    let pump_conn = client_conn.clone();
    let pump = std::thread::Builder::new()
        .name("manox-napi-pump".into())
        .spawn(move || {
            let rx = pump_conn.server_rx();
            while let Ok(msg) = rx.recv_blocking() {
                match serde_json::to_string(&msg) {
                    Ok(json) => {
                        let _ = tsfn.call(Ok(json), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                    Err(e) => {
                        eprintln!("manox-napi: failed to serialize FromServer: {e}");
                    }
                }
            }
        })
        .map_err(|e| napi::Error::from_reason(format!("failed to spawn pump thread: {e}")))?;

    // Send the Initialize handshake.
    let init_msg = FromClient::Request {
        id: MsgId::new("init"),
        call: manox_protocol::ClientCall::Initialize(Initialize {
            client_id: "vscode".into(),
            // VS Code has approval UI (sidebar), AskUserQuestion (answer_question
            // in sidebar), and PlanVerdict (plan_verdict in sidebar). No browser
            // or clipboard capabilities.
            capabilities: vec![
                HookKind::Approve,
                HookKind::AskUserQuestion,
                HookKind::PlanVerdict,
            ],
            sessions: vec![],
        }),
    };
    client_conn.send_to_server(init_msg);

    *slot = Some(ConnectionState {
        conn: client_conn,
        _pump: pump,
        _server: server,
    });
    Ok(())
}

/// Deliver a serialized `FromClient` JSON string to the AgentServer.
#[napi]
pub fn send_command(command: String) -> Result<()> {
    let msg: FromClient = serde_json::from_str(&command).map_err(|e| {
        napi::Error::from_reason(format!(
            "invalid FromClient JSON: {e}. Expected: \
             {{\"kind\":\"request\"|\"notification\"|\"reply\", ...}} \
             — see FromClient type in protocol.d.ts"
        ))
    })?;
    match conn_slot().as_ref() {
        Some(state) => {
            state.conn.send_to_server(msg);
            Ok(())
        }
        None => Err(napi::Error::from_reason(
            "actor not started; call start(callback) first",
        )),
    }
}

/// Tear down the agent connection and release the event callback. Safe to
/// call when the actor is not started; intended for host `deactivate`.
#[napi]
pub fn shutdown() {
    if let Some(state) = conn_slot().take() {
        state.conn.disconnect();
        // The pump thread exits when the channel is closed (disconnect closes
        // both directions). The AgentServer's tasks settle on the tokio runtime
        // when the connection drops.
    }
}
