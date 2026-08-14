//! Agent actor thread.
//!
//! Owns the gpui `HeadlessAppContext` and one `Thread` entity per session
//! (all thread-affine, `!Send`), processes commands delivered from the host
//! over an mpsc channel, and pushes serialized `ThreadEvent`s back through
//! an `EventSink`. Sessions are keyed by host-supplied ids and every
//! projected event carries its session id, so multiple host surfaces (chat
//! participant, sidebar) share the actor without cross-talk. The foreground
//! executor is driven with `run_until_parked` while any session's turn is
//! active, and the thread blocks on the command channel when idle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use gpui::{App, Entity, HeadlessAppContext, Subscription};

use agent::permission::{PermissionDecision, ToolAuthorizationResponse};
use agent::thread::ApprovalMode;
use agent::{Thread, ThreadEvent, ThreadId};

/// Sentinel command terminating the actor thread; see `ActorHandle::shutdown`.
const SHUTDOWN: &str = "__shutdown__";

/// Cloneable, `'static` event sink so subscription closures can outlive the
/// command that created them. Invoked from the actor thread, hence
/// `Send + Sync`. Transports wrap their callback in one; tests collect into
/// a buffer.
#[derive(Clone)]
pub struct EventSink(Arc<dyn Fn(String) + Send + Sync + 'static>);

impl EventSink {
    pub fn new(handler: impl Fn(String) + Send + Sync + 'static) -> Self {
        Self(Arc::new(handler))
    }

    pub fn emit(&self, json: String) {
        (self.0)(json);
    }
}

/// Host-side handle to the actor thread.
pub struct ActorHandle {
    tx: mpsc::Sender<String>,
    _thread: thread::JoinHandle<()>,
}

impl ActorHandle {
    pub fn send(&self, command: String) -> Result<(), String> {
        self.tx.send(command).map_err(|e| e.to_string())
    }

    /// Ask the actor thread to drain and exit, releasing the event sink.
    /// Returns without waiting for the thread so host teardown never blocks
    /// on in-flight work; the detached thread exits on the sentinel.
    pub fn shutdown(self) {
        let _ = self.tx.send(SHUTDOWN.to_string());
    }
}

/// Spawn the actor thread; returns a handle for command delivery.
pub fn start(sink: EventSink) -> Result<ActorHandle, std::io::Error> {
    let (tx, rx) = mpsc::channel::<String>();
    let handle = thread::Builder::new()
        .name("manox-agent".into())
        .spawn(move || run_actor(rx, sink))?;
    Ok(ActorHandle {
        tx,
        _thread: handle,
    })
}

struct SessionState {
    thread: Entity<Thread>,
    /// Keeps the `ThreadEvent` subscription alive for the session's lifetime.
    _subscription: Subscription,
    turn_active: Arc<AtomicBool>,
}

struct ActorState {
    sessions: HashMap<String, SessionState>,
    cwd: PathBuf,
}

impl ActorState {
    fn any_turn_active(&self) -> bool {
        self.sessions
            .values()
            .any(|s| s.turn_active.load(Ordering::SeqCst))
    }
}

fn run_actor(rx: mpsc::Receiver<String>, sink: EventSink) {
    let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
    cx.allow_parking();
    run_command_loop(&mut cx, rx, &sink);
    // `init` registered the ThreadStore entity in a process-global slot;
    // release it before the context drops so gpui's leaked-handle check
    // stays quiet across shutdown / window-reload cycles.
    agent::thread_store::drop_global_for_test();
}

fn run_command_loop(cx: &mut HeadlessAppContext, rx: mpsc::Receiver<String>, sink: &EventSink) {
    let mut state = ActorState {
        sessions: HashMap::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    };

    loop {
        let mut had_command = false;
        while let Ok(cmd) = rx.try_recv() {
            had_command = true;
            if !handle_command(cx, &mut state, sink, &cmd) {
                return;
            }
        }
        // Drive the foreground executor so pending async work (streaming,
        // tool callbacks) progresses and events are emitted.
        cx.run_until_parked();
        if had_command || state.any_turn_active() {
            // A turn is in flight; keep driving without blocking the channel.
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        // Idle: block until the host delivers a command.
        match rx.recv() {
            Ok(cmd) => {
                if !handle_command(cx, &mut state, sink, &cmd) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Returns `false` when the actor loop must exit (shutdown sentinel).
fn handle_command(
    cx: &mut HeadlessAppContext,
    state: &mut ActorState,
    sink: &EventSink,
    command: &str,
) -> bool {
    if command == SHUTDOWN {
        return false;
    }
    let cmd: serde_json::Value = match serde_json::from_str(command) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let Some(cmd_name) = cmd["cmd"].as_str() else {
        return true;
    };
    let session_id = cmd["sessionId"].as_str().map(str::to_string);
    match cmd_name {
        "init" => {
            if let Some(cwd) = cmd["cwd"].as_str() {
                state.cwd = PathBuf::from(cwd);
            }
            cx.update(agent::init);
            sink.emit(r#"{"type":"ready"}"#.to_string());
        }
        "create_session" => {
            let Some(id) = session_id.clone() else {
                sink.emit(error_json(None, "create_session requires sessionId"));
                return true;
            };
            let cwd = cmd["cwd"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| state.cwd.clone());
            let turn_active = Arc::new(AtomicBool::new(false));
            let thread = cx.update(|app| Thread::new_fresh(ThreadId(id.clone()), cwd, app));
            let subscription = cx.update(|app| {
                app.subscribe(&thread, {
                    let session_id = id.clone();
                    let turn_active = turn_active.clone();
                    let sink = sink.clone();
                    move |_entity: Entity<Thread>, ev: &ThreadEvent, _app: &mut App| {
                        match ev {
                            ThreadEvent::TurnStarted => turn_active.store(true, Ordering::SeqCst),
                            ThreadEvent::TurnFinished { .. } => {
                                turn_active.store(false, Ordering::SeqCst)
                            }
                            _ => {}
                        }
                        if let Some(json) =
                            crate::events::thread_event_to_json(ev, Some(&session_id))
                        {
                            sink.emit(json);
                        }
                    }
                })
            });
            if let Some(model) = agent::pi_providers::default_model() {
                cx.update(|app| {
                    thread.update(app, |t, cx| t.set_model(model, cx));
                });
            }
            state.sessions.insert(
                id.clone(),
                SessionState {
                    thread,
                    _subscription: subscription,
                    turn_active,
                },
            );
            sink.emit(serde_json::json!({"type": "session_created", "sessionId": id}).to_string());
        }
        "dispose_session" => {
            let Some(id) = session_id.clone() else {
                return true;
            };
            if let Some(session) = state.sessions.remove(&id) {
                if session.turn_active.load(Ordering::SeqCst) {
                    cx.update(|app| session.thread.update(app, |t, cx| t.cancel(cx)));
                }
                sink.emit(
                    serde_json::json!({"type": "session_disposed", "sessionId": id}).to_string(),
                );
            }
        }
        "submit" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let text = cmd["text"].as_str().unwrap_or_default().to_string();
            cx.update(|app| {
                session.thread.update(app, |t, cx| {
                    t.insert_user_message_with_ui_metadata(text, None, cx);
                    t.run_turn(cx);
                });
            });
        }),
        "cancel_turn" => with_session(state, session_id.as_deref(), sink, |session, _| {
            cx.update(|app| session.thread.update(app, |t, cx| t.cancel(cx)));
        }),
        "approve" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let id = cmd["id"].as_str().unwrap_or_default().to_string();
            let allow = cmd["allow"].as_bool().unwrap_or(false);
            let response = if allow {
                ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce)
            } else {
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny)
            };
            cx.update(|app| {
                session
                    .thread
                    .update(app, |t, cx| t.respond_authorization(&id, response, cx));
            });
        }),
        "set_approval_mode" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let mode = match cmd["mode"].as_str() {
                Some("danger") => ApprovalMode::Danger,
                _ => ApprovalMode::AutoPilot,
            };
            cx.update(|app| {
                session
                    .thread
                    .update(app, |t, cx| t.set_approval_mode(mode, cx));
            });
        }),
        "set_model" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let Some(id) = cmd["id"].as_str() else {
                return;
            };
            let registry = agent::pi_providers::global();
            match pi_extensions::model_ref::resolve_model_ref(&registry, id) {
                Some(model) => {
                    cx.update(|app| {
                        session.thread.update(app, |t, cx| t.set_model(model, cx));
                    });
                }
                None => sink.emit(error_json(session_id.as_deref(), "unknown model")),
            }
        }),
        "get_current_model" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let model = cx.update(|app| session.thread.read(app).model().cloned());
            let json = match model {
                Some(m) => serde_json::json!({
                    "type": "current_model",
                    "sessionId": session_id,
                    "id": m.id,
                    "name": agent::pi_providers::display_name(&m),
                }),
                None => serde_json::json!({
                    "type": "current_model",
                    "sessionId": session_id,
                    "id": null,
                }),
            };
            sink.emit(json.to_string());
        }),
        "get_usage" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let usage = cx.update(|app| session.thread.read(app).cumulative_token_usage());
            let json = serde_json::json!({
                "type": "usage",
                "sessionId": session_id,
                "usage": usage,
            });
            sink.emit(json.to_string());
        }),
        "list_models" => {
            let registry = agent::pi_providers::global();
            let models: Vec<serde_json::Value> = registry
                .models()
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "name": agent::pi_providers::display_name(m),
                        "provider": m.provider,
                    })
                })
                .collect();
            sink.emit(serde_json::json!({"type": "models", "models": models}).to_string());
        }
        _ => {}
    }
    true
}

/// Route a command to the session it names, reporting unknown sessions as
/// `error` events instead of silently dropping the command.
fn with_session(
    state: &mut ActorState,
    session_id: Option<&str>,
    sink: &EventSink,
    f: impl FnOnce(&SessionState, &EventSink),
) {
    let Some(id) = session_id else {
        sink.emit(error_json(None, "command requires sessionId"));
        return;
    };
    match state.sessions.get(id) {
        Some(session) => f(session, sink),
        None => sink.emit(error_json(Some(id), "unknown session")),
    }
}

fn error_json(session_id: Option<&str>, message: &str) -> String {
    serde_json::json!({
        "type": "error",
        "sessionId": session_id,
        "message": message,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

    /// Session-creating tests mutate `HOME` and initialize `OnceLock`
    /// globals, so they must not interleave with each other.
    static GLOBALS_LOCK: Mutex<()> = Mutex::new(());
    static HOME_ONCE: Once = Once::new();
    static INIT_ONCE: Once = Once::new();

    /// Point `HOME` at a throwaway directory so the thread db and provider
    /// config lookups stay out of the developer's real config. Never
    /// restored: the test process is disposable and provider registration
    /// reads `HOME` from a background thread.
    fn hermetic_home() {
        HOME_ONCE.call_once(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let home = std::env::temp_dir()
                .join(format!("manox-actor-test-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&home).unwrap();
            // SAFETY: test setup, serialized behind GLOBALS_LOCK.
            unsafe { std::env::set_var("HOME", home) };
        });
    }

    /// The tokio runtime and provider registry are process-wide `OnceLock`
    /// globals; initialize them exactly once, lightweight variants only
    /// (`agent::init` would also boot MCP/LSP/plugin subsystems).
    fn init_globals(cx: &mut HeadlessAppContext) {
        INIT_ONCE.call_once(|| {
            cx.update(agent::runtime::init);
            agent::pi_providers::init();
        });
    }

    fn collect_sink() -> (Arc<Mutex<Vec<String>>>, EventSink) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink_out = out.clone();
        (
            out.clone(),
            EventSink::new(move |json| sink_out.lock().unwrap().push(json)),
        )
    }

    fn types(out: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        out.lock()
            .unwrap()
            .iter()
            .map(|raw| {
                serde_json::from_str::<serde_json::Value>(raw).unwrap()["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn session_registry_routes_and_disposes() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        let mut state = ActorState {
            sessions: HashMap::new(),
            cwd: PathBuf::from("/"),
        };
        let (out, sink) = collect_sink();

        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        assert!(state.sessions.contains_key("s1"));
        assert!(types(&out).contains(&"session_created".to_string()));

        // Switching the approval policy surfaces as an approval_mode_changed
        // event for the session.
        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"set_approval_mode","sessionId":"s1","mode":"danger"}"#,
        );
        cx.run_until_parked();
        assert!(types(&out).contains(&"approval_mode_changed".to_string()));

        // A command for an unknown session surfaces as an error event.
        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"nope","text":"hi"}"#,
        );
        assert!(types(&out).contains(&"error".to_string()));

        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"dispose_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        assert!(!state.sessions.contains_key("s1"));
        assert!(types(&out).contains(&"session_disposed".to_string()));
        // Release every thread handle before the context drops so the gpui
        // leak detector sees a clean entity map.
        drop(state);
    }

    #[test]
    fn shutdown_sentinel_ends_the_loop() {
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = ActorState {
            sessions: HashMap::new(),
            cwd: PathBuf::from("/"),
        };
        let (out, sink) = collect_sink();
        // A command for an unknown session is handled (error event) and
        // keeps the loop alive; only the sentinel terminates it.
        assert!(handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"get_usage","sessionId":"ghost"}"#
        ));
        assert!(types(&out).contains(&"error".to_string()));
        assert!(!handle_command(&mut cx, &mut state, &sink, SHUTDOWN));
    }
}
