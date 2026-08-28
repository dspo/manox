//! The foreground pump that drives WebUI commands on the app main thread.
//!
//! Mirrors the tray pump: a single gpui-foreground task polls an inbound
//! channel every 100ms and dispatches inside `cx.update`. Per-connection
//! `ActorState`s live here (the app main thread), while the HTTP/WS workers
//! stay on the global tokio runtime. Events need no polling — the session
//! subscriptions fire their `EventSink` synchronously on the main thread and
//! push straight to each connection's sender task.

use std::sync::Arc;

use gpui::App;
use manox_session_core::session::{ActorState, EventSink, handle_command};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::ToMain;
use crate::bridge::{self, BridgeState, Outbound, ReadyKind};
use std::collections::HashMap;

use crate::proto_translate::{
    server_call_to_webview_json, server_note_to_webview_json, webview_to_from_client,
};
use manox_protocol::handshake::HookKind;
use manox_protocol::{
    ClientCall, FromClient, FromServer, InProcessConnection, Initialize, MsgId, RpcConnection,
    RpcError,
};

/// Command-direction poll period; event direction is push, so only commands
/// ride this timer.
const POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// One connected browser surface. `state` and the event subscription are
/// owned by the main thread; the WS worker holds the command sender and the
/// outbound fan-out.
pub(crate) struct Connection {
    pub(crate) id: u64,
    pub(crate) state: ActorState,
    pub(crate) sink: EventSink,
    pub(crate) bridge: BridgeState,
    cmd_rx: UnboundedReceiver<Value>,
}

impl Connection {
    fn new(id: u64, cmd_rx: UnboundedReceiver<Value>, outbound: Arc<Outbound>) -> Self {
        let pending_ready =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let bridge = BridgeState {
            pending_ready: pending_ready.clone(),
        };
        let sink = {
            let outbound = outbound.clone();
            EventSink::new(move |json| {
                bridge::on_event(&outbound, &pending_ready, &json);
            })
        };
        let cwd = agent::paths::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
        Self {
            id,
            state: ActorState::new(std::path::PathBuf::from(cwd)),
            sink,
            bridge,
            cmd_rx,
        }
    }
}

/// Detach every live session without cancelling turns: a browser disconnect
/// must not kill a turn the desktop app may still be showing.
fn detach_all(conn: &mut Connection) {
    let ids: Vec<String> = conn.state.sessions.keys().cloned().collect();
    for id in ids {
        let cmd = json!({"cmd": "detach_session", "sessionId": id});
        handle_command(&mut conn.state, &conn.sink, &cmd);
    }
}

/// Start the pump and expose its inbound channel to the server. Call once on
/// the gpui main thread at app startup.
pub fn spawn_pump(cx: &mut App) {
    let (main_tx, mut main_rx) = unbounded_channel::<ToMain>();
    crate::MAIN_CHANNEL
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .replace(main_tx);
    cx.spawn(async move |cx| {
        let mut conns: Vec<Connection> = Vec::new();
        loop {
            while let Ok(msg) = main_rx.try_recv() {
                match msg {
                    ToMain::Connect(handle) => {
                        conns.push(Connection::new(handle.id, handle.cmd_rx, handle.outbound));
                    }
                    ToMain::Disconnect(id) => {
                        cx.update(|_app| {
                            if let Some(conn) = conns.iter_mut().find(|c| c.id == id) {
                                detach_all(conn);
                            }
                        });
                        conns.retain(|c| c.id != id);
                    }
                }
            }
            for i in 0..conns.len() {
                let mut cmds = Vec::new();
                {
                    if let Some(conn) = conns.get_mut(i) {
                        while let Ok(cmd) = conn.cmd_rx.try_recv() {
                            cmds.push(cmd);
                        }
                    }
                }
                for cmd in cmds {
                    cx.update(|_app| {
                        if let Some(conn) = conns.get_mut(i) {
                            bridge::process_webui_msg(conn, &cmd);
                        }
                    });
                }
            }
            cx.background_executor().timer(POLL).await;
        }
    })
    .detach();
}

/// δ₁-b: shuttle one WebUI connection through the AgentServer. The WS worker
/// feeds `cmd_rx` (WebviewToHost JSON); the shuttle forwards each message as
/// `FromClient` and drains `FromServer` back through the reverse translations
/// into `bridge::on_event` (the legacy store's frame path). Runs on the agent
/// runtime. The AgentServer notices the disconnect only when its `client_rx`
/// closes (the pump rewire must not rely on a `Drop` here — `InProcessConnection`
/// has none; an explicit `disconnect()` is the pump's job on a WS close).
#[allow(dead_code)] // wired into spawn_pump by the δ₁-b pump rewire (next slice).
fn spawn_shuttle(
    client: InProcessConnection,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Value>,
    outbound: Arc<Outbound>,
    pending_ready: Arc<std::sync::Mutex<HashMap<String, (ReadyKind, String)>>>,
    client_id: String,
    cwd: String,
) -> tokio::task::JoinHandle<()> {
    agent::runtime::handle().spawn(async move {
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
                Some(cmd) = cmd_rx.recv() => {
                    // Share one synthesized id between the ready announce and
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
#[allow(dead_code)] // wired into spawn_shuttle (test-only here until pump rewire).
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
#[allow(dead_code)] // wired into spawn_shuttle (test-only here until pump rewire).
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
    use crate::bridge::ReadyKind;
    use gpui::HeadlessAppContext;
    use manox_protocol::in_process_pair;
    use manox_session_core::agent_server::AgentServer;
    use std::sync::{Mutex, Once};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

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

    fn init_globals(_cx: &mut HeadlessAppContext) {
        INIT_ONCE.call_once(|| {
            agent::runtime::init();
            agent::pi_providers::init();
        });
    }

    /// A live connection with a real `EventSink` wired into the session
    /// subscription, exposing its frame channel and outbound to the test.
    fn make_connection() -> (Connection, Arc<Outbound>, UnboundedReceiver<Value>) {
        let (frame_tx, frame_rx) = unbounded_channel();
        let (tick_tx, _tick_rx) = unbounded_channel();
        let outbound = Arc::new(Outbound::new(frame_tx, tick_tx));
        let (_cmd_tx, cmd_rx) = unbounded_channel();
        (
            Connection::new(1, cmd_rx, outbound.clone()),
            outbound,
            frame_rx,
        )
    }

    fn drain(rx: &mut UnboundedReceiver<Value>) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        frames
    }

    /// Whether a session event reached the wire, unwrapping batched `events`
    /// frames (session events coalesce into one frame per flush).
    fn has_event(frames: &[Value], ty: &str, sid: &str) -> bool {
        frames.iter().any(|f| {
            if f["type"] == ty && f["sessionId"] == sid {
                return true;
            }
            f["type"] == "events"
                && f["events"]
                    .as_array()
                    .is_some_and(|evs| evs.iter().any(|e| e["type"] == ty && e["sessionId"] == sid))
        })
    }

    /// A `new_session` message drives the full loop end to end: the webview
    /// message is translated, the actor creates a live thread, and the
    /// `session_created` event is consumed into a `session_ready(fresh)` on
    /// the connection's frame channel.
    #[test]
    fn new_session_round_trips_fresh_ready() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(|_cx| agent::thread_store::init());

        let (mut conn, outbound, mut frame_rx) = make_connection();
        let msg = json!({"type": "new_session", "sessionId": "s1"});
        cx.update(|_app| bridge::process_webui_msg(&mut conn, &msg));
        cx.run_until_parked();

        assert!(conn.state.sessions.contains_key("s1"));
        outbound.flush();
        let frames = drain(&mut frame_rx);
        assert!(
            frames.iter().any(|f| f["type"] == "session_ready"
                && f["sessionId"] == "s1"
                && f["kind"] == "fresh"),
            "frames: {frames:?}",
        );
        assert!(
            has_event(&frames, "current_model", "s1"),
            "picker model seed missing; frames: {frames:?}",
        );

        // Release every thread handle before the context drops so the gpui
        // leak detector sees a clean entity map.
        let ids: Vec<String> = conn.state.sessions.keys().cloned().collect();
        for id in ids {
            let cmd = json!({"cmd": "dispose_session", "sessionId": id});
            cx.update(|_app| handle_command(&mut conn.state, &conn.sink, &cmd));
        }
        cx.run_until_parked();
        drop(conn);
        agent::thread_store::drop_global_for_test();
    }

    /// An `open_thread` for a thread this connection created announces
    /// `restored` — the idempotent reopen path replays state instead of
    /// loading a second copy.
    #[test]
    fn open_thread_round_trips_restored_ready() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(|_cx| agent::thread_store::init());

        let (mut conn, outbound, mut frame_rx) = make_connection();
        cx.update(|_app| {
            bridge::process_webui_msg(
                &mut conn,
                &json!({"type": "new_session", "sessionId": "s1"}),
            );
        });
        cx.run_until_parked();
        outbound.flush();
        drain(&mut frame_rx);

        cx.update(|_app| {
            bridge::process_webui_msg(
                &mut conn,
                &json!({"type": "open_thread", "sessionId": "s1"}),
            );
        });
        cx.run_until_parked();
        outbound.flush();
        let frames = drain(&mut frame_rx);
        assert!(
            frames.iter().any(|f| f["type"] == "session_ready"
                && f["sessionId"] == "s1"
                && f["kind"] == "restored"),
            "frames: {frames:?}",
        );
        assert!(
            has_event(&frames, "current_model", "s1"),
            "picker model seed missing on restore; frames: {frames:?}",
        );

        let ids: Vec<String> = conn.state.sessions.keys().cloned().collect();
        for id in ids {
            let cmd = json!({"cmd": "dispose_session", "sessionId": id});
            cx.update(|_app| handle_command(&mut conn.state, &conn.sink, &cmd));
        }
        cx.run_until_parked();
        drop(conn);
        agent::thread_store::drop_global_for_test();
    }

    /// Buffered session events coalesce into one `events` frame on the 33ms
    /// flush the sender task performs — verified by flushing the outbound
    /// directly (the real flush path in `spawn_sender`).
    #[test]
    fn session_events_batch_into_one_frame() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(|_cx| agent::thread_store::init());

        let (mut conn, outbound, mut frame_rx) = make_connection();
        cx.update(|_app| {
            bridge::process_webui_msg(
                &mut conn,
                &json!({"type": "new_session", "sessionId": "s1"}),
            );
        });
        cx.run_until_parked();
        // The fresh thread's `approval_mode_changed` replays through the real
        // outbound; drain the buffer so only the events under test remain.
        outbound.flush();
        drain(&mut frame_rx);

        conn.bridge
            .pending_ready
            .lock()
            .unwrap()
            .insert("s1".to_string(), (ReadyKind::Fresh, "/".to_string()));
        bridge::on_event(
            &outbound,
            &conn.bridge.pending_ready,
            r#"{"type": "agent_text", "sessionId": "s1", "text": "hi"}"#,
        );
        bridge::on_event(
            &outbound,
            &conn.bridge.pending_ready,
            r#"{"type": "agent_text", "sessionId": "s1", "text": "again"}"#,
        );
        outbound.flush();
        let frames = drain(&mut frame_rx);
        assert_eq!(
            frames,
            vec![json!({"type": "events", "events": [
                {"type": "agent_text", "sessionId": "s1", "text": "hi"},
                {"type": "agent_text", "sessionId": "s1", "text": "again"},
            ]})],
        );

        let ids: Vec<String> = conn.state.sessions.keys().cloned().collect();
        for id in ids {
            let cmd = json!({"cmd": "dispose_session", "sessionId": id});
            cx.update(|_app| handle_command(&mut conn.state, &conn.sink, &cmd));
        }
        cx.run_until_parked();
        drop(conn);
        agent::thread_store::drop_global_for_test();
    }
    /// δ₁-b spine: WebviewToHost → forward → AgentServer → FromServer →
    /// reverse → on_event → frame, end-to-end over an in-process pair.
    #[test]
    fn shuttle_round_trips_new_session_to_ready() {
        let _g = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        INIT_ONCE.call_once(|| {
            agent::runtime::init();
            agent::pi_providers::init();
        });
        agent::thread_store::init();
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
                    agent::thread_store::drop_global_for_test();
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
