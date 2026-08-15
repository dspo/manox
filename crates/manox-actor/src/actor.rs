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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use gpui::{App, Entity, HeadlessAppContext, Subscription};
use serde_json::{Value, json};

use agent::language_model::MessageContent;
use agent::permission::{PermissionDecision, ToolAuthorizationResponse};
use agent::thread::ApprovalMode;
use agent::{
    Message, MessageUiMetadata, Thread, ThreadEvent, ThreadId, ThreadStore, ThreadStoreEvent,
};

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

/// Aggregated progress of one spawned sub-agent, mirrored from the
/// `Subagent*` events so an info-panel snapshot can list agents without
/// replaying the event stream.
#[derive(Clone)]
struct SubagentInfo {
    id: String,
    agent_type: String,
    description: String,
    tool_uses: u32,
    latest_activity: Option<String>,
    status: Value,
}

struct SessionState {
    thread: Entity<Thread>,
    /// Keeps the `ThreadEvent` subscription alive for the session's lifetime.
    _subscription: Subscription,
    turn_active: Arc<AtomicBool>,
    /// Sub-agent progress mirrored by the session's event subscription.
    subagents: Arc<Mutex<Vec<SubagentInfo>>>,
}

struct ActorState {
    sessions: HashMap<String, SessionState>,
    cwd: PathBuf,
    /// Thread id the host UI is currently showing. A turn that ends while its
    /// thread is unfocused marks itself unread so the list can badge it.
    focused: Arc<Mutex<Option<String>>>,
    /// Keeps the `ThreadStoreEvent` subscription alive for the actor's
    /// lifetime; established on the first `list_threads`.
    store_subscription: Option<Subscription>,
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
        focused: Arc::new(Mutex::new(None)),
        store_subscription: None,
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
            spawn_models_push(sink.clone());
        }
        "create_session" => {
            let Some(id) = session_id.clone() else {
                sink.emit(error_json(None, "create_session requires sessionId"));
                return true;
            };
            // The surface switches to the new conversation immediately, so it
            // counts as focused; otherwise its first finished turn would mark
            // it unread.
            *state.focused.lock().unwrap() = Some(id.clone());
            let cwd = cmd["cwd"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| state.cwd.clone());
            let turn_active = Arc::new(AtomicBool::new(false));
            let subagents = Arc::new(Mutex::new(Vec::new()));
            let thread = cx.update(|app| Thread::new_fresh(ThreadId(id.clone()), cwd, app));
            let subscription = cx.update(|app| {
                subscribe_thread(
                    app,
                    &thread,
                    id.clone(),
                    turn_active.clone(),
                    subagents.clone(),
                    state.focused.clone(),
                    sink.clone(),
                )
            });
            if let Some(model) = agent::pi_providers::default_model() {
                cx.update(|app| {
                    thread.update(app, |t, cx| t.set_model(model, cx));
                });
            }
            let persisted_mode = cx.update(|app| thread.read(app).approval_mode());
            state.sessions.insert(
                id.clone(),
                SessionState {
                    thread,
                    _subscription: subscription,
                    turn_active,
                    subagents,
                },
            );
            sink.emit(json!({"type": "session_created", "sessionId": id}).to_string());
            emit_persisted_approval_mode(persisted_mode, &id, sink);
        }
        "open_thread" => {
            let Some(id) = session_id.clone() else {
                sink.emit(error_json(None, "open_thread requires sessionId"));
                return true;
            };
            *state.focused.lock().unwrap() = Some(id.clone());
            // Idempotent reopen: a live session replays its snapshots
            // instead of loading a second copy of the thread.
            if let Some(session) = state.sessions.get(&id) {
                sink.emit(json!({"type": "session_created", "sessionId": id}).to_string());
                let (thread, subagents) = (session.thread.clone(), session.subagents.clone());
                let persisted_mode = cx.update(|app| thread.read(app).approval_mode());
                emit_persisted_approval_mode(persisted_mode, &id, sink);
                cx.update(|app| {
                    emit_history_and_info(app, &thread, &id, &subagents, sink);
                });
                return true;
            }
            let thread = cx.update(|app| {
                let store = agent::thread_store::global();
                store.update(app, |s, app| s.load_thread(&id, app))
            });
            let Some(thread) = thread else {
                sink.emit(error_json(Some(&id), "thread not found"));
                return true;
            };
            cx.update(|app| {
                let store = agent::thread_store::global();
                store.update(app, |s, cx| s.set_unread(&id, false, cx));
            });
            let turn_active = Arc::new(AtomicBool::new(false));
            let subagents = Arc::new(Mutex::new(Vec::new()));
            let subscription = cx.update(|app| {
                subscribe_thread(
                    app,
                    &thread,
                    id.clone(),
                    turn_active.clone(),
                    subagents.clone(),
                    state.focused.clone(),
                    sink.clone(),
                )
            });
            let persisted_mode = cx.update(|app| thread.read(app).approval_mode());
            state.sessions.insert(
                id.clone(),
                SessionState {
                    thread,
                    _subscription: subscription,
                    turn_active,
                    subagents,
                },
            );
            sink.emit(json!({"type": "session_created", "sessionId": id}).to_string());
            emit_persisted_approval_mode(persisted_mode, &id, sink);
        }
        "focus_thread" => {
            *state.focused.lock().unwrap() = session_id.clone();
            if let Some(id) = session_id.as_deref()
                && state.store_subscription.is_some()
            {
                let id = id.to_string();
                cx.update(|app| {
                    let store = agent::thread_store::global();
                    store.update(app, |s, cx| s.set_unread(&id, false, cx));
                });
            }
        }
        "list_threads" => {
            ensure_store_subscription(cx, state, sink);
            cx.update(|app| {
                let store = agent::thread_store::global();
                store.update(app, |s, cx| s.refresh(cx));
            });
            let threads = cx.update(|app| {
                let store = agent::thread_store::global();
                threads_snapshot(app, &store, &state.cwd)
            });
            sink.emit(json!({"type": "threads_updated", "threads": threads}).to_string());
        }
        "list_commands" => {
            let mut commands = Vec::new();
            if let Some(registry) = agent::command::try_global() {
                for (key, def) in registry.entries() {
                    commands.push(json!({
                        "name": key,
                        "description": def.description,
                        "kind": "command",
                        "argument_hint": def.argument_hint,
                    }));
                }
            }
            if let Some(registry) = agent::skill::try_global() {
                for (key, def) in registry.entries() {
                    commands.push(json!({
                        "name": key,
                        "description": def.description,
                        "kind": "skill",
                        "argument_hint": null,
                    }));
                }
            }
            sink.emit(json!({"type": "commands", "commands": commands}).to_string());
        }
        "thread_info" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let (thread, subagents) = (session.thread.clone(), session.subagents.clone());
            let id = session_id.clone().unwrap_or_default();
            cx.update(|app| {
                emit_thread_info(app, &thread, &id, &subagents, sink);
            });
        }),
        "dispose_session" => {
            let Some(id) = session_id.clone() else {
                return true;
            };
            {
                let mut focused = state.focused.lock().unwrap();
                if focused.as_deref() == Some(id.as_str()) {
                    *focused = None;
                }
            }
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
            let images: Vec<(String, String)> = cmd["images"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|img| {
                            Some((
                                img.get("data")?.as_str()?.to_string(),
                                img.get("mimeType")?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            cx.update(|app| {
                session.thread.update(app, |t, cx| {
                    let ui = MessageUiMetadata {
                        model_id: t.model().map(|m| m.id.clone()),
                        approval_mode: Some(t.approval_mode().as_i64()),
                        ..Default::default()
                    };
                    // Slash turns ride the registry; the bubble keeps the
                    // compact `/name args` form while the model sees the
                    // expanded body. Unmatched names fall through to a
                    // plain turn with the raw text.
                    if images.is_empty()
                        && let Some((name, args)) = parse_slash(&text)
                    {
                        let slash_ui = MessageUiMetadata {
                            display_text: Some(text.clone()),
                            ..ui.clone()
                        };
                        let command_hit = agent::command::try_global().is_some()
                            && t.submit_command(name, args, Some(slash_ui.clone()), cx);
                        let skill_hit = agent::skill::try_global().is_some()
                            && t.submit_skill(name, args, Some(slash_ui), cx);
                        if command_hit || skill_hit {
                            return;
                        }
                    }
                    let mut content = Vec::new();
                    if !text.trim().is_empty() {
                        content.push(MessageContent::Text(text));
                    }
                    content.extend(
                        images
                            .into_iter()
                            .map(|(data, mime_type)| MessageContent::Image { data, mime_type }),
                    );
                    if content.is_empty() {
                        return;
                    }
                    t.insert_user_message_with_content_and_ui_metadata(content, Some(ui), cx);
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
            let (usage, cost) = cx.update(|app| {
                let t = session.thread.read(app);
                (t.cumulative_token_usage(), t.cumulative_cost())
            });
            let json = serde_json::json!({
                "type": "usage",
                "sessionId": session_id,
                "usage": usage,
                "cost": cost,
            });
            sink.emit(json.to_string());
        }),
        "list_models" => sink.emit(models_snapshot().to_string()),
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

/// Restored threads keep their persisted policy; replay it on open so the
/// surface renders the right approval toggle without a round trip.
fn emit_persisted_approval_mode(mode: ApprovalMode, session_id: &str, sink: &EventSink) {
    sink.emit(
        serde_json::json!({
            "type": "approval_mode_changed",
            "sessionId": session_id,
            "mode": mode,
        })
        .to_string(),
    );
}

/// Shared `ThreadEvent` subscription for fresh and restored sessions alike.
/// Beyond projecting events onto the wire it maintains the thread-store list
/// bookkeeping (running / unread / pending-auth / errored), aggregates
/// sub-agent progress for the info panel, and answers `HistoryRestored` with
/// a full history snapshot.
fn subscribe_thread(
    app: &mut App,
    thread: &Entity<Thread>,
    session_id: String,
    turn_active: Arc<AtomicBool>,
    subagents: Arc<Mutex<Vec<SubagentInfo>>>,
    focused: Arc<Mutex<Option<String>>>,
    sink: EventSink,
) -> Subscription {
    app.subscribe(
        thread,
        move |entity: Entity<Thread>, ev: &ThreadEvent, app: &mut App| {
            match ev {
                ThreadEvent::TurnStarted => {
                    turn_active.store(true, Ordering::SeqCst);
                    let id = session_id.clone();
                    agent::thread_store::global().update(app, |s, cx| {
                        s.mark_running(&id, cx);
                        s.set_errored(&id, false, cx);
                    });
                }
                ThreadEvent::TurnFinished { .. } => {
                    turn_active.store(false, Ordering::SeqCst);
                    let unread = focused.lock().unwrap().as_deref() != Some(session_id.as_str());
                    let id = session_id.clone();
                    agent::thread_store::global().update(app, |s, cx| {
                        s.mark_idle(&id, cx);
                        s.mark_pending_auth(&id, false, cx);
                        if unread {
                            s.set_unread(&id, true, cx);
                        }
                    });
                }
                ThreadEvent::ToolCallAuthorization { .. } => {
                    let id = session_id.clone();
                    agent::thread_store::global()
                        .update(app, |s, cx| s.mark_pending_auth(&id, true, cx));
                }
                ThreadEvent::Error(_) => {
                    let id = session_id.clone();
                    agent::thread_store::global().update(app, |s, cx| s.set_errored(&id, true, cx));
                }
                ThreadEvent::SubagentStarted {
                    id,
                    subagent_type,
                    description,
                    ..
                } => {
                    let mut list = subagents.lock().unwrap();
                    if !list.iter().any(|a| &a.id == id) {
                        list.push(SubagentInfo {
                            id: id.clone(),
                            agent_type: subagent_type.clone(),
                            description: description.clone(),
                            tool_uses: 0,
                            latest_activity: None,
                            status: json!("running"),
                        });
                    }
                }
                ThreadEvent::SubagentProgress {
                    id,
                    tool_uses,
                    latest_activity,
                    status,
                    ..
                } => {
                    if let Some(entry) = subagents.lock().unwrap().iter_mut().find(|a| &a.id == id)
                    {
                        entry.tool_uses = *tool_uses;
                        entry.latest_activity = latest_activity.clone();
                        entry.status = serde_json::to_value(status).unwrap_or(Value::Null);
                    }
                }
                ThreadEvent::HistoryRestored => {
                    emit_history_and_info(app, &entity, &session_id, &subagents, &sink);
                }
                _ => {}
            }
            if let Some(json) = crate::events::thread_event_to_json(ev, Some(&session_id)) {
                sink.emit(json);
            }
        },
    )
}

/// Replay a thread's full history plus an info snapshot — the payload an
/// opened thread lands with, whether freshly loaded or already live.
fn emit_history_and_info(
    app: &App,
    thread: &Entity<Thread>,
    session_id: &str,
    subagents: &Arc<Mutex<Vec<SubagentInfo>>>,
    sink: &EventSink,
) {
    let messages = strip_messages_for_wire(thread.read(app).messages());
    sink.emit(
        json!({
            "type": "thread_history",
            "sessionId": session_id,
            "messages": messages,
        })
        .to_string(),
    );
    emit_thread_info(app, thread, session_id, subagents, sink);
}

/// Info-panel snapshot: worktree, plan, cumulative usage/cost, pending-auth
/// depth and live sub-agents, followed by an async branch lookup.
fn emit_thread_info(
    app: &App,
    thread: &Entity<Thread>,
    session_id: &str,
    subagents: &Arc<Mutex<Vec<SubagentInfo>>>,
    sink: &EventSink,
) {
    let agents: Vec<Value> = subagents
        .lock()
        .unwrap()
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "agent_type": a.agent_type,
                "description": a.description,
                "tool_uses": a.tool_uses,
                "latest_activity": a.latest_activity,
                "status": a.status,
            })
        })
        .collect();
    let t = thread.read(app);
    let worktree_path = t.worktree_path().map(str::to_string);
    let branch_dir = worktree_path
        .clone()
        .unwrap_or_else(|| t.cwd().to_string_lossy().into_owned());
    sink.emit(
        json!({
            "type": "thread_info",
            "sessionId": session_id,
            "info": {
                "worktree_path": worktree_path,
                "plan": t.persisted_plan().and_then(|p| serde_json::to_value(p).ok()),
                "usage": t.cumulative_token_usage(),
                "per_model_usage": t.per_model_token_usage(),
                "cost": t.cumulative_cost(),
                "pending_auth_count": t.pending_auth_entries().len(),
                "agents": agents,
            },
        })
        .to_string(),
    );
    spawn_branch_lookup(session_id.to_string(), branch_dir.clone(), sink.clone());
    spawn_git_stats(session_id.to_string(), branch_dir, sink.clone());
}

/// `threads_updated` item list, scoped to this project's live sessions.
fn threads_snapshot(app: &App, store: &Entity<ThreadStore>, cwd: &Path) -> Value {
    let store = store.read(app);
    let cwd = cwd.to_string_lossy();
    Value::Array(
        store
            .summaries()
            .iter()
            .filter(|s| !s.archived && s.project == cwd.as_ref())
            .map(|s| {
                json!({
                    "id": s.id,
                    "title": s.display_title(),
                    "updated_at": s.interacted_at,
                    "running": store.is_running(&s.id),
                    "unread": s.has_unread,
                    "errored": s.errored,
                    "pending_auth": store.pending_auth_contains(&s.id),
                    "model_id": s.model_id,
                })
            })
            .collect(),
    )
}

/// Subscribe once to store updates so list mutations (title generation,
/// unread sidecars, running flips) push fresh snapshots without polling.
fn ensure_store_subscription(
    cx: &mut HeadlessAppContext,
    state: &mut ActorState,
    sink: &EventSink,
) {
    if state.store_subscription.is_some() {
        return;
    }
    let cwd = state.cwd.clone();
    let sink = sink.clone();
    state.store_subscription = Some(cx.update(|app| {
        let store = agent::thread_store::global();
        app.subscribe(
            &store,
            move |store: Entity<ThreadStore>, _ev: &ThreadStoreEvent, app: &mut App| {
                let threads = threads_snapshot(app, &store, &cwd);
                sink.emit(json!({"type": "threads_updated", "threads": threads}).to_string());
            },
        )
    }));
}

/// Tool-result content beyond this many characters is truncated before it
/// crosses the wire.
const TOOL_RESULT_WIRE_LIMIT: usize = 100_000;

/// Serialize messages for wire transport with heavy payloads deflated:
/// image blocks become `{mime_type, byte_len}` placeholders (multi-MB
/// base64 would stall postMessage) and tool results are capped.
fn strip_messages_for_wire(messages: &[Message]) -> Value {
    let mut value = serde_json::to_value(messages).unwrap_or(Value::Array(Vec::new()));
    let Some(list) = value.as_array_mut() else {
        return value;
    };
    for msg in list {
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content {
            let Value::Object(map) = block else {
                continue;
            };
            if let Some(image) = map.get_mut("Image").and_then(Value::as_object_mut) {
                let byte_len = image
                    .get("data")
                    .and_then(Value::as_str)
                    .map(base64_byte_len)
                    .unwrap_or(0);
                image.remove("data");
                image.insert("byte_len".into(), json!(byte_len));
            } else if let Some(result) = map.get_mut("ToolResult").and_then(Value::as_object_mut)
                && let Some(len) = result.get("content").and_then(Value::as_str).map(str::len)
                && len > TOOL_RESULT_WIRE_LIMIT
            {
                let truncated: String = result["content"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(TOOL_RESULT_WIRE_LIMIT)
                    .collect();
                result.insert("content".into(), json!(truncated));
            }
        }
    }
    value
}

fn base64_byte_len(b64: &str) -> u64 {
    let quarters = (b64.len() as u64) * 3 / 4;
    let padding = b64.bytes().rev().take_while(|&b| b == b'=').count() as u64;
    quarters.saturating_sub(padding)
}

/// Split a `/name args` invocation; an empty name is not a slash turn.
fn parse_slash(text: &str) -> Option<(&str, &str)> {
    let body = text.strip_prefix('/')?;
    let (name, args) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    let name = name.trim();
    (!name.is_empty()).then(|| (name, args.trim_start()))
}

/// Resolve the checked-out branch off the actor thread; the panel shows a
/// placeholder until the `branch` event lands.
fn spawn_branch_lookup(session_id: String, dir: String, sink: EventSink) {
    agent::runtime::handle().spawn_blocking(move || {
        if let Some(branch) = git_branch(&dir) {
            sink.emit(
                json!({"type": "branch", "sessionId": session_id, "branch": branch}).to_string(),
            );
        }
    });
}

/// Working-tree change counts for the info card's branch row; every failure
/// mode degrades to zeros rather than blocking the snapshot.
fn spawn_git_stats(session_id: String, dir: String, sink: EventSink) {
    agent::runtime::handle().spawn_blocking(move || {
        sink.emit(
            json!({
                "type": "git_stats",
                "sessionId": session_id,
                "stats": git_stats(&dir),
            })
            .to_string(),
        );
    });
}

fn git_stats(dir: &str) -> Value {
    let (mut added, mut deleted) = (0u64, 0u64);
    if let Some(out) = std::process::Command::new("git")
        .args(["diff", "--numstat", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((a, d)) = parse_numstat_line(line) {
                added += a;
                deleted += d;
            }
        }
    }
    let untracked = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .count()
        })
        .unwrap_or(0);
    json!({ "added": added, "deleted": deleted, "untracked": untracked })
}

/// One `git diff --numstat` line: "added<TAB>deleted<TAB>path"; binary
/// files report "-" in both count columns and contribute nothing.
fn parse_numstat_line(line: &str) -> Option<(u64, u64)> {
    let mut parts = line.splitn(3, '\t');
    let added = parts.next()?.parse().ok()?;
    let deleted = parts.next()?.parse().ok()?;
    Some((added, deleted))
}

/// Wire shape shared by the `list_models` response and the post-init push.
fn model_json(model: &pi::types::Model) -> Value {
    json!({
        "id": model.id,
        "name": agent::pi_providers::display_name(model),
        "provider": model.provider,
        "api": model.api,
        "context_window": model.context_window,
    })
}

fn models_snapshot() -> Value {
    let models: Vec<Value> = agent::pi_providers::global()
        .models()
        .iter()
        .map(model_json)
        .collect();
    json!({ "type": "models", "models": models })
}

/// Push a `models` snapshot once the one-shot provider registration
/// completes: surfaces that listed before then saw an empty registry and
/// never retried, leaving the model picker permanently disabled.
fn spawn_models_push(sink: EventSink) {
    agent::runtime::handle().spawn(async move {
        agent::pi_providers::wait_ready().await;
        sink.emit(models_snapshot().to_string());
    });
}

fn git_branch(dir: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(dir)
        .output()
        .ok()?;
    if out.status.success() {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    // Detached HEAD: surface the short sha instead of an empty branch.
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
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
            focused: Arc::new(Mutex::new(None)),
            store_subscription: None,
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

        // A fresh session opens focused and replays its approval policy, so
        // the surface's toggle starts on the thread's actual mode and the
        // first finished turn does not badge the thread unread.
        assert_eq!(state.focused.lock().unwrap().as_deref(), Some("s1"));
        let modes: Vec<String> = out
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_str::<serde_json::Value>(raw).unwrap())
            .filter(|v| v["type"] == "approval_mode_changed")
            .map(|v| v["mode"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(modes, vec!["autopilot".to_string()]);

        // Switching the approval policy surfaces as an approval_mode_changed
        // event for the session.
        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"set_approval_mode","sessionId":"s1","mode":"danger"}"#,
        );
        cx.run_until_parked();
        let modes: Vec<String> = out
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_str::<serde_json::Value>(raw).unwrap())
            .filter(|v| v["type"] == "approval_mode_changed")
            .map(|v| v["mode"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(modes, vec!["autopilot".to_string(), "danger".to_string()]);

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
            focused: Arc::new(Mutex::new(None)),
            store_subscription: None,
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

    /// Pump the executor until an event type has landed `count` times;
    /// mirrors the actor loop's 5ms cadence so tokio-side wakers get
    /// re-polled between parks.
    fn pump_until(
        out: &Arc<Mutex<Vec<String>>>,
        cx: &mut HeadlessAppContext,
        wanted: &str,
        count: usize,
    ) -> bool {
        for _ in 0..400 {
            cx.run_until_parked();
            if types(out).iter().filter(|t| t.as_str() == wanted).count() >= count {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn state_with(cwd: PathBuf) -> ActorState {
        ActorState {
            sessions: HashMap::new(),
            cwd,
            focused: Arc::new(Mutex::new(None)),
            store_subscription: None,
        }
    }

    #[test]
    fn parses_slash_invocations() {
        assert_eq!(
            parse_slash("/deploy prod now"),
            Some(("deploy", "prod now"))
        );
        assert_eq!(parse_slash("/deploy"), Some(("deploy", "")));
        assert_eq!(parse_slash("/deploy\t--force"), Some(("deploy", "--force")));
        assert_eq!(parse_slash("deploy"), None);
        assert_eq!(parse_slash("/ args"), None);
        assert_eq!(parse_slash("/"), None);
    }

    #[test]
    fn strips_images_and_caps_tool_results_for_wire() {
        use agent::language_model::LanguageModelToolResult;

        let long_output = "x".repeat(TOOL_RESULT_WIRE_LIMIT + 10);
        let messages = vec![
            Message::user_with_content(vec![MessageContent::Image {
                data: "aGVsbG8=".into(), // "hello" — 5 bytes
                mime_type: "image/png".into(),
            }]),
            Message::assistant(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "t1".into(),
                tool_name: Arc::from("bash"),
                is_error: false,
                content: long_output,
            })]),
        ];

        let value = strip_messages_for_wire(&messages);
        let image = &value[0]["content"][0]["Image"];
        assert!(image.get("data").is_none());
        assert_eq!(image["mime_type"], "image/png");
        assert_eq!(image["byte_len"], 5);

        let result = &value[1]["content"][0]["ToolResult"];
        assert_eq!(
            result["content"].as_str().map(str::len),
            Some(TOOL_RESULT_WIRE_LIMIT)
        );
        assert_eq!(value[1]["role"], "assistant");
    }

    #[test]
    fn list_threads_emits_project_scoped_snapshot() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        // A cwd no session belongs to: the snapshot must come back empty no
        // matter what earlier tests left in the hermetic home.
        let mut state = state_with(PathBuf::from("/no/such/project"));
        let (out, sink) = collect_sink();

        handle_command(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        assert!(state.store_subscription.is_some());
        assert!(pump_until(&out, &mut cx, "threads_updated", 1));

        let events: Vec<Value> = out
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_str(raw).unwrap())
            .collect();
        let snapshot = events
            .iter()
            .find(|e| e["type"] == "threads_updated")
            .expect("threads_updated emitted");
        assert_eq!(snapshot["threads"], Value::Array(Vec::new()));

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn list_commands_reports_registry_shape() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        // Idempotent: whichever test reaches the registries first loads
        // them from the (empty) hermetic home.
        agent::command::init();
        agent::skill::init();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let (out, sink) = collect_sink();

        handle_command(&mut cx, &mut state, &sink, r#"{"cmd":"list_commands"}"#);

        let events: Vec<Value> = out
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_str(raw).unwrap())
            .collect();
        let payload = events
            .iter()
            .find(|e| e["type"] == "commands")
            .expect("commands event emitted");
        assert!(payload["commands"].is_array());
        drop(state);
    }

    #[test]
    fn submit_attaches_metadata_images_and_slash_fallthrough() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        // Turn lifecycle events maintain store bookkeeping, so the store
        // global must exist before any turn starts.
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/"));
        let (out, sink) = collect_sink();

        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"hello","images":[{"data":"aGVsbG8=","mimeType":"image/png"}]}"#,
        );

        let messages = cx.update(|app| state.sessions["s1"].thread.read(app).messages().to_vec());
        assert_eq!(messages.len(), 1);
        let ui = messages[0].ui.as_ref().expect("ui metadata attached");
        assert_eq!(ui.approval_mode, Some(ApprovalMode::AutoPilot.as_i64()));
        assert!(
            messages[0]
                .content
                .iter()
                .any(|c| matches!(c, MessageContent::Text(t) if t == "hello"))
        );
        assert!(messages[0].content.iter().any(
            |c| matches!(c, MessageContent::Image { mime_type, .. } if mime_type.as_str() == "image/png")
        ));

        assert!(types(&out).contains(&"turn_started".to_string()));

        // An unmatched slash command falls through to a plain message
        // carrying the raw text. The first turn is still in flight, so no
        // second turn starts and the assertions stay synchronous.
        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"/no-such-command arg"}"#,
        );
        let messages = cx.update(|app| state.sessions["s1"].thread.read(app).messages().to_vec());
        let last = messages.last().expect("fallthrough message inserted");
        assert!(
            last.content
                .iter()
                .any(|c| matches!(c, MessageContent::Text(t) if t == "/no-such-command arg"))
        );
        assert!(
            last.ui
                .as_ref()
                .and_then(|ui| ui.display_text.as_ref())
                .is_none()
        );

        // Dispose cancels the in-flight turn; the engine shuts down on
        // `Thread::drop`.
        handle_command(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"dispose_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn pushes_models_snapshot_after_provider_registration() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        let (out, sink) = collect_sink();

        spawn_models_push(sink);
        assert!(pump_until(&out, &mut cx, "models", 1));
        let event: Value =
            serde_json::from_str(&out.lock().unwrap()[0]).expect("models event is valid json");
        // The hermetic home registers no providers; the snapshot still lands
        // so waiting surfaces can leave their disabled state.
        assert!(event["models"].is_array());
    }

    #[test]
    fn model_json_exposes_wire_fields() {
        let model = pi::types::Model {
            provider: "anthropic".into(),
            api: "anthropic".into(),
            id: "claude-sonnet-4-6".into(),
            context_window: 200_000,
            max_tokens: 8192,
            thinking: pi::types::ThinkingKind::None,
            metadata: HashMap::new(),
        };
        let value = model_json(&model);
        assert_eq!(value["id"], "claude-sonnet-4-6");
        assert_eq!(value["provider"], "anthropic");
        assert_eq!(value["api"], "anthropic");
        assert_eq!(value["context_window"], 200_000);
        assert!(value["name"].is_string());
    }

    #[test]
    fn parses_numstat_lines() {
        assert_eq!(parse_numstat_line("12\t3\tsrc/main.rs"), Some((12, 3)));
        assert_eq!(parse_numstat_line("0\t0\tnew.rs"), Some((0, 0)));
        // Binary files report "-" counts.
        assert_eq!(parse_numstat_line("-\t-\tlogo.png"), None);
        assert_eq!(parse_numstat_line(""), None);
    }

    #[test]
    fn git_stats_outside_a_repo_is_zero() {
        let dir = std::env::temp_dir().join(format!("manox-git-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stats = git_stats(dir.to_str().unwrap());
        assert_eq!(stats["added"], 0);
        assert_eq!(stats["deleted"], 0);
        assert_eq!(stats["untracked"], 0);
    }
}
