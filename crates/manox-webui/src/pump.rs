//! The foreground pump that drives WebUI commands on the app main thread.
//!
//! Each WS connection is handed to the AgentServer over an in-process pair;
//! a tokio `spawn_shuttle` task bridges the browser's WebviewToHost /
//! HostToWebview to the protocol.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::unbounded_channel;

use crate::ToMain;
use crate::bridge::{self, Outbound, ReadyKind};
use std::collections::HashMap;

use crate::proto_translate::{
    server_call_to_webview_json, server_note_to_webview_json, webview_to_from_client,
};
use manox_protocol::handshake::HookKind;
use manox_protocol::{
    ClientCall, FromClient, FromServer, InProcessConnection, Initialize, MsgId, RpcConnection,
    RpcError, in_process_pair,
};
use manox_session_core::agent_server::AgentServer;

/// Start the pump and expose its inbound channel to the WS server. Call once
/// at app startup. Each WS connection is handed to the AgentServer over an
/// in-process pair; a tokio `spawn_shuttle` task bridges the browser's
/// WebviewToHost/HostToWebview to the protocol.
pub fn spawn_server() {
    let (main_tx, mut main_rx) = unbounded_channel::<ToMain>();
    crate::MAIN_CHANNEL
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .replace(main_tx);
    let cwd = crate::bridge::resolve_cwd();
    let server = AgentServer::new(std::path::PathBuf::from(cwd.clone()));
    manox_agent::runtime::handle().spawn(async move {
        let mut shuttles: HashMap<
            u64,
            (
                tokio::task::JoinHandle<()>,
                manox_protocol::InProcessConnection,
            ),
        > = HashMap::new();
        while let Some(msg) = main_rx.recv().await {
            match msg {
                ToMain::Connect(handle) => {
                    let (client_conn, server_conn) = in_process_pair();
                    server.accept(Arc::new(server_conn));
                    let pending_ready = Arc::new(std::sync::Mutex::new(HashMap::new()));
                    let shuttle = spawn_shuttle(
                        client_conn.clone(),
                        handle.cmd_rx,
                        handle.outbound.clone(),
                        pending_ready,
                        format!("webui-{}", handle.id),
                        cwd.clone(),
                    );
                    shuttles.insert(handle.id, (shuttle, client_conn));
                }
                ToMain::Disconnect(id) => {
                    if let Some((shuttle, conn)) = shuttles.remove(&id) {
                        conn.disconnect();
                        shuttle.abort();
                    }
                }
            }
        }
    });
}

/// δ₁-b: shuttle one WebUI connection through the AgentServer. The WS worker
/// feeds `cmd_rx` (WebviewToHost JSON); the shuttle forwards each message as
/// `FromClient` and drains `FromServer` back through the reverse translations
/// into `bridge::on_event` (the legacy store's frame path). Runs on the agent
/// runtime. The AgentServer notices the disconnect only when its `client_rx`
/// closes (the pump rewire must not rely on a `Drop` here — `InProcessConnection`
/// has none; an explicit `disconnect()` is the pump's job on a WS close).
fn spawn_shuttle(
    client: InProcessConnection,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Value>,
    outbound: Arc<Outbound>,
    pending_ready: Arc<std::sync::Mutex<HashMap<String, (ReadyKind, String)>>>,
    client_id: String,
    cwd: String,
) -> tokio::task::JoinHandle<()> {
    manox_agent::runtime::handle().spawn(async move {
        client.send_to_server(FromClient::Request {
            id: MsgId::new("init"),
            call: ClientCall::Initialize(Initialize {
                client_id,
                capabilities: vec![HookKind::Approve, HookKind::PlanVerdict, HookKind::AskUserQuestion],
                sessions: vec![],
            }),
        });
        let rx = client.server_rx();
        // Request id → session id, so a query Response can carry its session
        // onto the note the legacy store expects (the Response itself has none).
        let pending_sessions: std::sync::Mutex<HashMap<String, Option<String>>> =
            std::sync::Mutex::new(HashMap::new());
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    // A None means the WS worker closed (disconnect): tear down
                    // the pair so the AgentServer's serve loop exits + removes the client.
                    let Some(cmd) = cmd else { client.disconnect(); break; };
                    // the CreateSession so SessionCreated resolves pending_ready.
                    let ready = crate::proto_translate::webview_ready_metadata(&cmd, &cwd);
                    let id_override = ready.as_ref().map(|(id, _, _)| id.clone());
                    if let Some((rid, rkind, rcwd)) = ready {
                        pending_ready.lock().unwrap().insert(rid, (rkind, rcwd));
                    }
                    for fc in webview_to_from_client(&cmd, &cwd, id_override.as_deref()) {
                        if let FromClient::Request { id, call } = &fc {
                            pending_sessions.lock().unwrap().insert(id.0.clone(), session_id_of(call));
                        }
                        client.send_to_server(fc);
                    }
                }
                msg = rx.recv() => {
                    let Ok(msg) = msg else { break; };
                    let json = match msg {
                        FromServer::Notification { note } => server_note_to_webview_json(&note),
                        FromServer::Request { call, .. } => server_call_to_webview_json(&call),
                        FromServer::Response { id, outcome } => response_to_note(&id, &outcome, &pending_sessions),
                    };
                    if let Some(v) = json {
                        bridge::on_event(&outbound, &pending_ready, &v.to_string());
                    }
                }
            }
        }
    })
}

/// The session id a `ClientCall` Request targets (for Response→note mapping).
fn session_id_of(call: &ClientCall) -> Option<String> {
    match call {
        ClientCall::OpenSession { session_id }
        | ClientCall::GetUsage { session_id }
        | ClientCall::GetCurrentModel { session_id }
        | ClientCall::ThreadInfo { session_id } => Some(session_id.clone()),
        _ => None,
    }
}

/// Map a query Response onto the legacy store frame its id implies, recovering
/// the session id from the tracked Request.
fn response_to_note(
    id: &MsgId,
    outcome: &Result<Value, RpcError>,
    pending: &std::sync::Mutex<HashMap<String, Option<String>>>,
) -> Option<Value> {
    let sid = pending
        .lock()
        .unwrap()
        .get(id.0.as_str())
        .cloned()
        .flatten();
    let v = outcome.as_ref().ok()?;
    Some(match id.0.as_str() {
        "list_models" => json!({"type":"models","models": v}),
        "list_threads" => json!({"type":"threads_updated","threads": v}),
        "list_commands" => json!({"type":"commands","commands": v}),
        "get_usage" => {
            json!({"type":"usage","sessionId": sid, "usage": v["usage"], "cost": v["cost"]})
        }
        // The server routes the full ServerNote::ThreadInfo *before* responding
        // Ok({}); the note already delivered the panel, so skip the empty-ack
        // Response (it would clobber the real info with `{}`).
        "thread_info" => return None,
        "cur_model" => {
            json!({"type":"current_model","sessionId": sid, "id": v["id"], "name": v["name"]})
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use manox_protocol::in_process_pair;
    use manox_session_core::agent_server::AgentServer;
    use std::sync::{Mutex, Once};
    use tokio::sync::mpsc::unbounded_channel;

    /// Session-driving tests mutate `HOME` and initialize `OnceLock`
    /// globals, so they must not interleave with each other.
    static GLOBALS_LOCK: Mutex<()> = Mutex::new(());
    static HOME_ONCE: Once = Once::new();
    static INIT_ONCE: Once = Once::new();

    fn hermetic_home() {
        HOME_ONCE.call_once(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let home = std::env::temp_dir()
                .join(format!("manox-webui-test-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&home).unwrap();
            // SAFETY: test setup, serialized behind GLOBALS_LOCK.
            unsafe { std::env::set_var("HOME", home) };
        });
    }

    /// δ₁-b spine: WebviewToHost → forward → AgentServer → FromServer →
    /// reverse → on_event → frame, end-to-end over an in-process pair.
    #[test]
    fn shuttle_round_trips_new_session_to_ready() {
        let _g = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        INIT_ONCE.call_once(|| {
            manox_agent::runtime::init();
            manox_agent::provider_glue::init();
        });
        manox_agent::thread_store::init();
        let server = AgentServer::new(std::path::PathBuf::from("/"));
        let (client_conn, server_conn) = in_process_pair();
        server.accept(std::sync::Arc::new(server_conn));
        let (frame_tx, mut frame_rx) = unbounded_channel::<Value>();
        let (tick_tx, _tick) = unbounded_channel::<()>();
        let outbound = Arc::new(Outbound::new(frame_tx, tick_tx));
        let pending_ready = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = unbounded_channel::<Value>();
        let _shuttle = spawn_shuttle(
            client_conn,
            cmd_rx,
            outbound.clone(),
            pending_ready,
            "test".into(),
            "/".into(),
        );
        cmd_tx
            .send(json!({"type":"new_session","sessionId":"s1"}))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            outbound.flush();
            while let Ok(f) = frame_rx.try_recv() {
                if f["type"] == "session_ready" && f["sessionId"] == "s1" {
                    manox_agent::thread_store::drop_global_for_test();
                    return;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session_ready never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
