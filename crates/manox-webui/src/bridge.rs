//! The wire bridge between a browser surface and `manox-session-core`.
//!
//! The browser is a dumb terminal: `WebviewToHost` messages are translated
//! into actor commands and driven on the app main thread against the same
//! `Entity<Thread>`s the desktop Workspace operates. Events flow back through
//! one `EventSink` per connection: session events are coalesced into a 33ms
//! `events` frame (mirroring the vscode sidebar's batch timer), `thread_info`
//! and `session_ready` bypass the buffer (flush-then-send keeps ordering),
//! and the global snapshots (`models`/`threads`/`commands`) unwrap into their
//! own frames — the same envelope the vscode `sidebarProvider` posts.

use std::collections::HashMap;
use std::sync::Mutex;

use manox_session_core::session::handle_command;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::pump::Connection;

/// Events accumulated into one frame. Mirrors `EVENT_BATCH_INTERVAL_MS` in
/// the vscode sidebar provider.
pub(crate) const BATCH_MS: u64 = 33;

/// Whether a `session_ready` announces a brand-new conversation or a restored
/// persisted thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadyKind {
    Fresh,
    Restored,
}

impl ReadyKind {
    fn wire(&self) -> &'static str {
        match self {
            ReadyKind::Fresh => "fresh",
            ReadyKind::Restored => "restored",
        }
    }
}

/// Per-connection wire state owned by the main-thread pump.
pub(crate) struct BridgeState {
    /// `session_created` is consumed into a `session_ready`; entries record
    /// the announce metadata for the create/open the connection itself
    /// initiated. Shared with the connection's `EventSink` closure.
    pub pending_ready: std::sync::Arc<Mutex<HashMap<String, (ReadyKind, String)>>>,
}

/// Event fan-out shared with the connection's sender task: session events
/// accumulate in the batch buffer, frames the frontend must see promptly
/// (`thread_info`, `session_ready`, global snapshots) go straight to the
/// frame channel. All methods are callable from the app main thread, where
/// the `EventSink` fires.
pub(crate) struct Outbound {
    batch: Mutex<BatchState>,
    frame_tx: UnboundedSender<Value>,
    tick_tx: UnboundedSender<()>,
}

struct BatchState {
    pending: Vec<Value>,
    armed: bool,
}

impl Outbound {
    pub(crate) fn new(frame_tx: UnboundedSender<Value>, tick_tx: UnboundedSender<()>) -> Self {
        Self {
            batch: Mutex::new(BatchState {
                pending: Vec::new(),
                armed: false,
            }),
            frame_tx,
            tick_tx,
        }
    }

    /// Queue one session event. The first event after a flush arms a 33ms
    /// flush deadline in the sender task, so a streaming burst coalesces
    /// into a single frame.
    pub(crate) fn batch(&self, ev: Value) {
        let mut st = self.batch.lock().unwrap();
        if st.pending.is_empty() && !st.armed {
            st.armed = true;
            let _ = self.tick_tx.send(());
        }
        st.pending.push(ev);
    }

    /// Drain the buffer into an `events` frame, then send `frame` right
    /// after it — the bypass path keeps `thread_info`/`session_ready` from
    /// overtaking buffered events.
    pub(crate) fn flush_then_send(&self, frame: Value) {
        self.send_events_frame();
        let _ = self.frame_tx.send(frame);
    }

    /// Send a standalone frame immediately (global snapshots).
    pub(crate) fn send_frame(&self, frame: Value) {
        let _ = self.frame_tx.send(frame);
    }

    /// Drain the buffer if anything accumulated (called by the sender task
    /// on its 33ms deadline).
    pub(crate) fn flush(&self) {
        self.send_events_frame();
    }

    fn send_events_frame(&self) {
        let events = {
            let mut st = self.batch.lock().unwrap();
            st.armed = false;
            std::mem::take(&mut st.pending)
        };
        if !events.is_empty() {
            let _ = self
                .frame_tx
                .send(json!({"type": "events", "events": events}));
        }
    }
}

/// Event-sink callback: route one actor event JSON to the browser.
pub(crate) fn on_event(
    outbound: &Outbound,
    pending_ready: &Mutex<HashMap<String, (ReadyKind, String)>>,
    json: &str,
) {
    let Ok(ev) = serde_json::from_str::<Value>(json) else {
        return;
    };
    let ty = ev["type"].as_str().unwrap_or("");
    if ev["sessionId"].as_str().is_none() {
        // Global events unwrap into their own frames; `ready` is consumed
        // (the app host is already initialized, so no init handshake).
        match ty {
            "models" => outbound.send_frame(ev),
            "threads_updated" => {
                outbound.send_frame(json!({"type": "threads", "threads": ev["threads"]}))
            }
            "commands" => outbound.send_frame(ev),
            "error" => {
                outbound.send_frame(json!({"type": "global_error", "message": ev["message"]}))
            }
            _ => {}
        }
        return;
    }
    if ty == "session_created" {
        // Consume the create/open confirmation into the session_ready the
        // webview expects first, matching the vscode sidebar.
        let sid = ev["sessionId"].as_str().unwrap_or("").to_string();
        let ready = pending_ready.lock().unwrap().remove(&sid);
        if let Some((kind, cwd)) = ready {
            outbound.flush_then_send(json!({
                "type": "session_ready",
                "sessionId": sid,
                "cwd": cwd,
                "kind": kind.wire(),
            }));
        }
        return;
    }
    if ty == "thread_info" {
        // Info-panel snapshot bypasses the batch so a reload gets fresh
        // state immediately; buffered events flush first to keep order.
        outbound.flush_then_send(ev);
        return;
    }
    outbound.batch(ev);
}

/// The default project directory for new sessions: the most recently
/// registered project the thread store knows, falling back to `$HOME`.
fn resolve_cwd(app: &gpui::App) -> String {
    if let Some(store) = agent::thread_store::try_global() {
        let known = store.read(app).known_projects();
        if let Some(project) = known.last() {
            return project.clone();
        }
    }
    agent::paths::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string())
}

/// Translate one `WebviewToHost` message into the actor commands it maps to,
/// plus the `session_ready` announce metadata for create/open commands.
/// Pure pass-through commands map one-to-one; `new_session` /
/// `plan_execute_fresh` orchestrate a sequence. Kept side-effect-free so the
/// wire mapping is unit-testable against the `messages.ts` contract.
fn translate(msg: &Value, cwd: &str) -> (Vec<Value>, Option<(String, ReadyKind, String)>) {
    let ty = msg["type"].as_str().unwrap_or("");
    let sid = msg["sessionId"].as_str().map(str::to_string);
    match ty {
        "new_session" => {
            let id = msg["sessionId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut cmds = vec![json!({"cmd": "create_session", "sessionId": id, "cwd": cwd})];
            if let Some(model) = msg["modelId"].as_str() {
                cmds.push(json!({"cmd": "set_model", "sessionId": id, "id": model}));
            }
            // The picker's currentModelId is seeded by the authoritative
            // `current_model` response; `set_model` alone emits no wire
            // event, so the query must follow any model override.
            cmds.push(json!({"cmd": "get_current_model", "sessionId": id}));
            if msg["text"].as_str().is_some()
                || msg["images"].as_array().is_some_and(|a| !a.is_empty())
            {
                let text = msg["text"].as_str().unwrap_or_default();
                let images = msg.get("images").cloned().unwrap_or(Value::Null);
                cmds.push(
                    json!({"cmd": "submit", "sessionId": id, "text": text, "images": images}),
                );
            }
            (cmds, Some((id, ReadyKind::Fresh, cwd.to_string())))
        }
        "open_thread" => {
            let Some(id) = sid else {
                return (vec![], None);
            };
            (
                vec![
                    json!({"cmd": "open_thread", "sessionId": id}),
                    // Seed the picker's currentModelId from the restored
                    // thread's persisted model.
                    json!({"cmd": "get_current_model", "sessionId": id}),
                ],
                Some((id, ReadyKind::Restored, cwd.to_string())),
            )
        }
        "plan_execute_fresh" => {
            let Some(plan_file) = msg["planFile"].as_str() else {
                return (vec![], None);
            };
            let Some(cwd) = msg["cwd"].as_str() else {
                return (vec![], None);
            };
            let mut cmds = Vec::new();
            if let Some(old) = sid {
                cmds.push(json!({"cmd": "archive_thread", "sessionId": old, "archived": true}));
            }
            let fresh = uuid::Uuid::new_v4().to_string();
            cmds.push(json!({"cmd": "create_session", "sessionId": fresh, "cwd": cwd}));
            cmds.push(
                json!({"cmd": "plan_seed_execution", "sessionId": fresh, "planFile": plan_file}),
            );
            (cmds, Some((fresh, ReadyKind::Fresh, cwd.to_string())))
        }
        "submit" => (
            vec![json!({
                "cmd": "submit", "sessionId": sid, "text": msg["text"],
                "images": msg["images"], "clientId": msg["clientId"],
            })],
            None,
        ),
        "steer" => (
            vec![json!({
                "cmd": "steer", "sessionId": sid, "clientId": msg["clientId"],
                "text": msg["text"], "images": msg["images"],
            })],
            None,
        ),
        "drop_queued" => (
            vec![json!({
                "cmd": "drop_queued", "sessionId": sid, "clientId": msg["clientId"],
            })],
            None,
        ),
        "approve" => (
            vec![json!({
                "cmd": "approve", "sessionId": sid, "id": msg["id"], "allow": msg["allow"],
            })],
            None,
        ),
        "cancel" => (vec![json!({"cmd": "cancel_turn", "sessionId": sid})], None),
        "set_model" => (
            vec![json!({"cmd": "set_model", "sessionId": sid, "id": msg["id"]})],
            None,
        ),
        "set_reasoning_effort" => (
            vec![json!({
                "cmd": "set_reasoning_effort", "sessionId": sid, "effort": msg["effort"],
            })],
            None,
        ),
        "set_approval_mode" => (
            vec![json!({
                "cmd": "set_approval_mode", "sessionId": sid, "mode": msg["mode"],
            })],
            None,
        ),
        "set_plan_mode" => (
            vec![json!({
                "cmd": "set_plan_mode", "sessionId": sid, "enabled": msg["enabled"],
            })],
            None,
        ),
        "plan_verdict" => (
            vec![json!({
                "cmd": "plan_verdict", "sessionId": sid, "choice": msg["choice"],
            })],
            None,
        ),
        "goal" => (
            vec![json!({
                "cmd": "goal", "sessionId": sid, "action": msg["action"],
                "objective": msg["objective"], "budget": msg["budget"],
            })],
            None,
        ),
        "stop_background_task" => (
            vec![json!({
                "cmd": "stop_background_task", "sessionId": sid, "taskId": msg["taskId"],
            })],
            None,
        ),
        "answer_question" => (
            vec![json!({
                "cmd": "answer_question", "sessionId": sid, "id": msg["id"],
                "answers": msg["answers"], "response": msg["response"],
            })],
            None,
        ),
        "request_usage" => (vec![json!({"cmd": "get_usage", "sessionId": sid})], None),
        "request_thread_info" => (vec![json!({"cmd": "thread_info", "sessionId": sid})], None),
        "focus_thread" => (
            vec![json!({"cmd": "focus_thread", "sessionId": msg["sessionId"]})],
            None,
        ),
        "archive_thread" => (
            vec![json!({
                "cmd": "archive_thread", "sessionId": sid, "archived": msg["archived"],
            })],
            None,
        ),
        "pin_thread" => (
            vec![json!({
                "cmd": "pin_thread", "sessionId": sid, "pinned": msg["pinned"],
            })],
            None,
        ),
        "list_threads" => (vec![json!({"cmd": "list_threads"})], None),
        "list_commands" => (vec![json!({"cmd": "list_commands"})], None),
        "request_models" => (vec![json!({"cmd": "list_models"})], None),
        _ => (vec![], None),
    }
}

/// Translate one `WebviewToHost` message into actor commands and drive them
/// on the app main thread, pre-registering the `session_ready` announce.
pub(crate) fn process_webui_msg(app: &mut gpui::App, conn: &mut Connection, msg: &Value) {
    let cwd = resolve_cwd(app);
    let (cmds, ready) = translate(msg, &cwd);
    if let Some((id, kind, cwd)) = ready {
        conn.bridge
            .pending_ready
            .lock()
            .unwrap()
            .insert(id, (kind, cwd));
    }
    for cmd in cmds {
        handle_command(app, &mut conn.state, &conn.sink, &cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn outbound() -> (
        Arc<Outbound>,
        UnboundedReceiver<Value>,
        tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
        let (frame_tx, frame_rx) = unbounded_channel();
        let (tick_tx, tick_rx) = unbounded_channel();
        (
            Arc::new(Outbound::new(frame_tx, tick_tx)),
            frame_rx,
            tick_rx,
        )
    }

    fn drain(rx: &mut UnboundedReceiver<Value>) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        frames
    }

    /// One translate case: the webview message, the expected commands, and
    /// the optional `session_ready` announce metadata.
    type Case = (Value, Value, Option<(String, ReadyKind, String)>);

    /// Every `WebviewToHost` message maps to its actor command(s) verbatim;
    /// the assertion mirrors the `messages.ts` contract so a drift between
    /// the two bridge implementations fails this test.
    #[test]
    fn translate_maps_each_message_type() {
        let cwd = "/proj";
        let cases: Vec<Case> = vec![
            (
                json!({"type": "cancel", "sessionId": "s1"}),
                json!([{"cmd": "cancel_turn", "sessionId": "s1"}]),
                None,
            ),
            (
                json!({"type": "request_usage", "sessionId": "s1"}),
                json!([{"cmd": "get_usage", "sessionId": "s1"}]),
                None,
            ),
            (
                json!({"type": "request_thread_info", "sessionId": "s1"}),
                json!([{"cmd": "thread_info", "sessionId": "s1"}]),
                None,
            ),
            (
                json!({"type": "set_model", "sessionId": "s1", "id": "m1"}),
                json!([{"cmd": "set_model", "sessionId": "s1", "id": "m1"}]),
                None,
            ),
            (
                json!({"type": "set_reasoning_effort", "sessionId": "s1", "effort": "high"}),
                json!([{"cmd": "set_reasoning_effort", "sessionId": "s1", "effort": "high"}]),
                None,
            ),
            (
                json!({"type": "set_approval_mode", "sessionId": "s1", "mode": "danger-full-access"}),
                json!([{"cmd": "set_approval_mode", "sessionId": "s1", "mode": "danger-full-access"}]),
                None,
            ),
            (
                json!({"type": "set_plan_mode", "sessionId": "s1", "enabled": true}),
                json!([{"cmd": "set_plan_mode", "sessionId": "s1", "enabled": true}]),
                None,
            ),
            (
                json!({"type": "plan_verdict", "sessionId": "s1", "choice": "approve"}),
                json!([{"cmd": "plan_verdict", "sessionId": "s1", "choice": "approve"}]),
                None,
            ),
            (
                json!({"type": "stop_background_task", "sessionId": "s1", "taskId": "t1"}),
                json!([{"cmd": "stop_background_task", "sessionId": "s1", "taskId": "t1"}]),
                None,
            ),
            (
                json!({"type": "answer_question", "sessionId": "s1", "id": "q1", "answers": [["a", "b"]], "response": "r"}),
                json!([{"cmd": "answer_question", "sessionId": "s1", "id": "q1", "answers": [["a", "b"]], "response": "r"}]),
                None,
            ),
            (
                json!({"type": "archive_thread", "sessionId": "s1", "archived": true}),
                json!([{"cmd": "archive_thread", "sessionId": "s1", "archived": true}]),
                None,
            ),
            (
                json!({"type": "pin_thread", "sessionId": "s1", "pinned": false}),
                json!([{"cmd": "pin_thread", "sessionId": "s1", "pinned": false}]),
                None,
            ),
            (
                json!({"type": "focus_thread", "sessionId": "s1"}),
                json!([{"cmd": "focus_thread", "sessionId": "s1"}]),
                None,
            ),
            (
                json!({"type": "list_threads"}),
                json!([{"cmd": "list_threads"}]),
                None,
            ),
            (
                json!({"type": "list_commands"}),
                json!([{"cmd": "list_commands"}]),
                None,
            ),
            (
                json!({"type": "request_models"}),
                json!([{"cmd": "list_models"}]),
                None,
            ),
        ];
        for (msg, expected_cmds, expected_ready) in cases {
            let (cmds, ready) = translate(&msg, cwd);
            assert_eq!(Value::Array(cmds), expected_cmds, "msg: {msg}");
            assert_eq!(ready, expected_ready, "msg: {msg}");
        }
    }

    /// `submit`/`steer`/`approve` carry their payload through unchanged.
    #[test]
    fn translate_passes_through_turn_payloads() {
        let cwd = "/proj";
        let (cmds, ready) = translate(
            &json!({
                "type": "submit", "sessionId": "s1", "text": "hi",
                "images": [{"path": "/a.png"}], "clientId": "c1",
            }),
            cwd,
        );
        assert_eq!(
            Value::Array(cmds),
            json!([{"cmd": "submit", "sessionId": "s1", "text": "hi",
                    "images": [{"path": "/a.png"}], "clientId": "c1"}]),
        );
        assert_eq!(ready, None);

        let (cmds, ready) = translate(
            &json!({"type": "steer", "sessionId": "s1", "clientId": "c1", "text": "more"}),
            cwd,
        );
        assert_eq!(
            Value::Array(cmds),
            json!([{"cmd": "steer", "sessionId": "s1", "clientId": "c1", "text": "more", "images": null}]),
        );
        assert_eq!(ready, None);

        let (cmds, ready) = translate(
            &json!({"type": "approve", "sessionId": "s1", "id": "req1", "allow": false}),
            cwd,
        );
        assert_eq!(
            Value::Array(cmds),
            json!([{"cmd": "approve", "sessionId": "s1", "id": "req1", "allow": false}]),
        );
        assert_eq!(ready, None);
    }

    /// `new_session` synthesizes an id when the webview leaves it out and
    /// pre-registers the `session_ready(fresh)` announce; a `modelId` becomes
    /// a `set_model` and a non-empty composer payload a `submit`.
    #[test]
    fn translate_new_session_orchestrates() {
        let cwd = "/proj";
        let (cmds, ready) = translate(
            &json!({"type": "new_session", "text": "hi", "modelId": "m1"}),
            cwd,
        );
        assert_eq!(cmds.len(), 4);
        assert_eq!(cmds[0]["cmd"], "create_session");
        assert_eq!(
            cmds[1],
            json!({"cmd": "set_model", "sessionId": cmds[0]["sessionId"], "id": "m1"})
        );
        assert_eq!(
            cmds[2],
            json!({"cmd": "get_current_model", "sessionId": cmds[0]["sessionId"]})
        );
        assert_eq!(cmds[3]["cmd"], "submit");
        let (id, kind, cwd_out) = ready.expect("fresh announce");
        assert_eq!(kind, ReadyKind::Fresh);
        assert_eq!(cwd_out, cwd);
        assert_eq!(id, cmds[0]["sessionId"].as_str().unwrap());

        // Empty composer: create + model seed, still announces fresh.
        let (cmds, ready) = translate(&json!({"type": "new_session", "sessionId": "s1"}), cwd);
        assert_eq!(
            Value::Array(cmds),
            json!([
                {"cmd": "create_session", "sessionId": "s1", "cwd": cwd},
                {"cmd": "get_current_model", "sessionId": "s1"},
            ]),
        );
        assert_eq!(
            ready,
            Some(("s1".to_string(), ReadyKind::Fresh, cwd.to_string()))
        );
    }

    #[test]
    fn translate_open_thread_announces_restored() {
        let (cmds, ready) = translate(&json!({"type": "open_thread", "sessionId": "s1"}), "/proj");
        assert_eq!(
            Value::Array(cmds),
            json!([
                {"cmd": "open_thread", "sessionId": "s1"},
                {"cmd": "get_current_model", "sessionId": "s1"},
            ]),
        );
        assert_eq!(
            ready,
            Some(("s1".to_string(), ReadyKind::Restored, "/proj".to_string()))
        );
    }

    #[test]
    fn translate_plan_execute_fresh_archives_then_seeds() {
        let (cmds, ready) = translate(
            &json!({"type": "plan_execute_fresh", "sessionId": "s1", "planFile": "/p.md", "cwd": "/w"}),
            "/ignored",
        );
        assert_eq!(cmds.len(), 3);
        assert_eq!(
            cmds[0],
            json!({"cmd": "archive_thread", "sessionId": "s1", "archived": true}),
        );
        assert_eq!(cmds[1]["cmd"], "create_session");
        assert_eq!(cmds[1]["cwd"], "/w");
        assert_eq!(
            cmds[2],
            json!({"cmd": "plan_seed_execution", "sessionId": cmds[1]["sessionId"], "planFile": "/p.md"}),
        );
        let (id, kind, cwd) = ready.expect("fresh announce");
        assert_eq!(kind, ReadyKind::Fresh);
        assert_eq!(cwd, "/w");
        assert_eq!(id, cmds[1]["sessionId"].as_str().unwrap());
    }

    /// A batch of session events coalesces into one `events` frame on flush;
    /// the batch arms the 33ms tick exactly once per burst.
    #[test]
    fn outbound_batches_into_one_frame() {
        let (out, mut frame_rx, mut tick_rx) = outbound();
        out.batch(json!({"type": "agent_text", "text": "a"}));
        out.batch(json!({"type": "agent_text", "text": "b"}));
        assert!(tick_rx.try_recv().is_ok(), "first event arms the tick");
        assert!(tick_rx.try_recv().is_err(), "no re-arm while pending");

        out.flush();
        let frames = drain(&mut frame_rx);
        assert_eq!(
            frames,
            vec![json!({"type": "events", "events": [
                {"type": "agent_text", "text": "a"},
                {"type": "agent_text", "text": "b"},
            ]})],
        );

        // Nothing buffered: a second flush emits nothing.
        out.flush();
        assert!(drain(&mut frame_rx).is_empty());
    }

    /// `flush_then_send` drains buffered events first, so a bypass frame
    /// (`thread_info`/`session_ready`) never overtakes in-flight events.
    #[test]
    fn outbound_bypass_flushes_buffered_events_first() {
        let (out, mut frame_rx, _) = outbound();
        out.batch(json!({"type": "agent_text", "text": "queued"}));
        out.flush_then_send(json!({"type": "thread_info", "sessionId": "s1", "info": {}}));
        assert_eq!(
            drain(&mut frame_rx),
            vec![
                json!({"type": "events", "events": [{"type": "agent_text", "text": "queued"}]}),
                json!({"type": "thread_info", "sessionId": "s1", "info": {}}),
            ],
        );
    }

    /// Global snapshots unwrap into their own frames; `threads_updated` and
    /// `error` are re-enveloped to match the vscode host messages.
    #[test]
    fn on_event_global_snapshots_unwrap() {
        let (out, mut frame_rx, _) = outbound();
        let pending = Mutex::new(HashMap::new());
        on_event(
            &out,
            &pending,
            r#"{"type": "models", "models": [{"id": "m1"}]}"#,
        );
        on_event(
            &out,
            &pending,
            r#"{"type": "threads_updated", "threads": [{"id": "t1"}]}"#,
        );
        on_event(
            &out,
            &pending,
            r#"{"type": "commands", "commands": [{"name": "c1"}]}"#,
        );
        on_event(&out, &pending, r#"{"type": "error", "message": "boom"}"#);
        assert_eq!(
            drain(&mut frame_rx),
            vec![
                json!({"type": "models", "models": [{"id": "m1"}]}),
                json!({"type": "threads", "threads": [{"id": "t1"}]}),
                json!({"type": "commands", "commands": [{"name": "c1"}]}),
                json!({"type": "global_error", "message": "boom"}),
            ],
        );
    }

    /// `session_created` is consumed into a `session_ready` when the connection
    /// itself initiated the create/open; announce kind matches the request.
    #[test]
    fn on_event_consumes_session_created_into_session_ready() {
        let (out, mut frame_rx, _) = outbound();
        let pending = Mutex::new(HashMap::new());
        pending
            .lock()
            .unwrap()
            .insert("s1".to_string(), (ReadyKind::Fresh, "/proj".to_string()));
        on_event(
            &out,
            &pending,
            r#"{"type": "session_created", "sessionId": "s1"}"#,
        );
        assert_eq!(
            drain(&mut frame_rx),
            vec![
                json!({"type": "session_ready", "sessionId": "s1", "cwd": "/proj", "kind": "fresh"})
            ],
        );

        // A create this connection did not initiate (no pending entry) is
        // dropped, not re-announced.
        let (out, mut frame_rx, _) = outbound();
        on_event(
            &out,
            &pending,
            r#"{"type": "session_created", "sessionId": "other"}"#,
        );
        assert!(drain(&mut frame_rx).is_empty());
    }

    /// Ordinary session events are buffered; `thread_info` bypasses and
    /// flushes the buffer ahead of it.
    #[test]
    fn on_event_batches_session_events() {
        let (out, mut frame_rx, _) = outbound();
        let pending = Mutex::new(HashMap::new());
        on_event(
            &out,
            &pending,
            r#"{"type": "agent_text", "sessionId": "s1", "text": "hi"}"#,
        );
        assert!(drain(&mut frame_rx).is_empty(), "session event is buffered");
        on_event(
            &out,
            &pending,
            r#"{"type": "thread_info", "sessionId": "s1", "info": {"model": "m1"}}"#,
        );
        assert_eq!(
            drain(&mut frame_rx),
            vec![
                json!({"type": "events", "events": [{"type": "agent_text", "sessionId": "s1", "text": "hi"}]}),
                json!({"type": "thread_info", "sessionId": "s1", "info": {"model": "m1"}}),
            ],
        );
    }
}
