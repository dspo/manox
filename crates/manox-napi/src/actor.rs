//! Agent actor thread.
//!
//! Owns the gpui `HeadlessAppContext` and the `Thread` entity (both are
//! thread-affine, `!Send`), processes commands delivered from the Node host
//! over an mpsc channel, and pushes serialized `ThreadEvent`s back via a
//! `ThreadsafeFunction`. The foreground executor is driven with
//! `run_until_parked` while a turn is active, and the thread blocks on the
//! command channel when idle.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use gpui::{App, Entity, HeadlessAppContext, Subscription};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use agent::permission::{PermissionDecision, ToolAuthorizationResponse};
use agent::{Thread, ThreadEvent, ThreadId};

/// Host-side handle to the actor thread.
pub struct ActorHandle {
    tx: mpsc::Sender<String>,
}

impl ActorHandle {
    pub fn send(&self, command: String) -> napi::Result<()> {
        self.tx
            .send(command)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
}

/// Spawn the actor thread; returns a handle for command delivery.
pub fn start(event_sink: ThreadsafeFunction<Vec<String>>) -> napi::Result<ActorHandle> {
    let (tx, rx) = mpsc::channel::<String>();
    thread::Builder::new()
        .name("manox-agent".into())
        .spawn(move || run_actor(rx, event_sink))
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    Ok(ActorHandle { tx })
}

struct ActorState {
    thread: Option<Entity<Thread>>,
    cwd: PathBuf,
    turn_active: Arc<AtomicBool>,
}

fn run_actor(rx: mpsc::Receiver<String>, event_sink: ThreadsafeFunction<Vec<String>>) {
    let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
    cx.allow_parking();
    let mut state = ActorState {
        thread: None,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        turn_active: Arc::new(AtomicBool::new(false)),
    };
    let mut subscription: Option<Subscription> = None;

    loop {
        let mut had_command = false;
        while let Ok(cmd) = rx.try_recv() {
            had_command = true;
            if !handle_command(&mut cx, &mut state, &mut subscription, &event_sink, &cmd) {
                return;
            }
        }
        // Drive the foreground executor so pending async work (streaming,
        // tool callbacks) progresses and events are emitted.
        cx.run_until_parked();
        if had_command || state.turn_active.load(Ordering::SeqCst) {
            // A turn is in flight; keep driving without blocking the channel.
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        // Idle: block until the host delivers a command.
        match rx.recv() {
            Ok(cmd) => {
                if !handle_command(&mut cx, &mut state, &mut subscription, &event_sink, &cmd) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

fn handle_command(
    cx: &mut HeadlessAppContext,
    state: &mut ActorState,
    subscription: &mut Option<Subscription>,
    event_sink: &ThreadsafeFunction<Vec<String>>,
    command: &str,
) -> bool {
    let cmd: serde_json::Value = match serde_json::from_str(command) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let Some(cmd_name) = cmd["cmd"].as_str() else {
        return true;
    };
    match cmd_name {
        "init" => {
            if let Some(cwd) = cmd["cwd"].as_str() {
                state.cwd = PathBuf::from(cwd);
            }
            cx.update(agent::init);
            emit(
                event_sink,
                &serde_json::json!({"type": "ready"}).to_string(),
            );
        }
        "create_session" => {
            let turn_active = state.turn_active.clone();
            let thread = cx.update(|app| {
                Thread::new_fresh(
                    ThreadId(uuid::Uuid::new_v4().to_string()),
                    state.cwd.clone(),
                    app,
                )
            });
            let sub = cx.update(|app| {
                app.subscribe(&thread, {
                    let sink = event_sink.clone();
                    let turn_active = turn_active.clone();
                    move |_entity: Entity<Thread>, ev: &ThreadEvent, _app: &mut App| {
                        match ev {
                            ThreadEvent::TurnStarted => turn_active.store(true, Ordering::SeqCst),
                            ThreadEvent::TurnFinished { .. } => {
                                turn_active.store(false, Ordering::SeqCst)
                            }
                            _ => {}
                        }
                        if let Some(json) = crate::events::thread_event_to_json(ev) {
                            let _ = sink.call(Ok(vec![json]), ThreadsafeFunctionCallMode::Blocking);
                        }
                    }
                })
            });
            *subscription = Some(sub);
            state.thread = Some(thread.clone());
            if let Some(model) = agent::pi_providers::default_model() {
                cx.update(|app| {
                    thread.update(app, |t, cx| t.set_model(model, cx));
                });
            }
            emit(
                event_sink,
                &serde_json::json!({"type": "session_created"}).to_string(),
            );
        }
        "submit" => {
            let Some(thread) = state.thread.clone() else {
                return true;
            };
            let text = cmd["text"].as_str().unwrap_or_default().to_string();
            cx.update(|app| {
                thread.update(app, |t, cx| {
                    t.insert_user_message_with_ui_metadata(text, None, cx);
                    t.run_turn(cx);
                });
            });
        }
        "get_usage" => {
            if let Some(thread) = state.thread.clone() {
                let usage = cx.update(|app| thread.read(app).cumulative_token_usage());
                let json = serde_json::json!({"type": "usage", "usage": usage});
                emit(event_sink, &json.to_string());
            }
        }
        "approve" => {
            let Some(thread) = state.thread.clone() else {
                return true;
            };
            let id = cmd["id"].as_str().unwrap_or_default().to_string();
            let allow = cmd["allow"].as_bool().unwrap_or(false);
            let response = if allow {
                ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce)
            } else {
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny)
            };
            cx.update(|app| {
                thread.update(app, |t, cx| t.respond_authorization(&id, response, cx));
            });
        }
        _ => {}
    }
    true
}

fn emit(event_sink: &ThreadsafeFunction<Vec<String>>, payload: &str) {
    let _ = event_sink.call(
        Ok(vec![payload.to_string()]),
        ThreadsafeFunctionCallMode::Blocking,
    );
}
