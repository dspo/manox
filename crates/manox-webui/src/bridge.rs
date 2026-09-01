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

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

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
pub(crate) fn resolve_cwd() -> String {
    if let Some(store) = manox_agent::thread_store::try_global() {
        let known = store.read(|s| s.known_projects().to_vec());
        if let Some(project) = known.last() {
            return project.clone();
        }
    }
    manox_agent::paths::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string())
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
