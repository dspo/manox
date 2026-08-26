//! Session orchestration engine.
//!
//! Owns one `Thread` entity per session (all thread-affine, `!Send`) and
//! processes JSON commands through `handle_command` on the host's `App`,
//! pushing serialized `ThreadEvent`s back through an `EventSink`. Sessions
//! are keyed by host-supplied ids and every projected event carries its
//! session id, so multiple host surfaces (chat participant, sidebar, WebUI
//! bridge) share the engine without cross-talk. The host drives the
//! foreground executor itself; the engine only mutates the handed-in `App`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui::{App, Entity, Subscription};
use serde_json::{Value, json};

use agent::language_model::MessageContent;
use agent::permission::{PermissionDecision, ToolAuthorizationResponse};
use agent::thread::PermissionMode;
use agent::{
    Message, MessageUiMetadata, Thread, ThreadEvent, ThreadId, ThreadStore, ThreadStoreEvent,
};

/// Cloneable, `'static` event sink so subscription closures can outlive the
/// command that created them. Invoked from the driving thread, hence
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

/// Plan-file identity recorded from a `PlanReady`, so a later `plan_verdict`
/// can seed execution without a round-trip.
#[derive(Clone)]
struct PendingPlan {
    plan_file: String,
}
/// A submission parked while a turn is running; drained into one batched
/// follow-up turn when the turn settles (`run_turn` joins the texts with
/// blank lines, matching the gpui host's `flush_queued_follow_ups`).
struct QueuedSubmit {
    client_id: String,
    text: String,
    images: Vec<(String, String)>,
    ui: agent::MessageUiMetadata,
}

pub struct SessionState {
    pub thread: Entity<Thread>,
    /// Keeps the `ThreadEvent` subscription alive for the session's lifetime.
    _subscription: Subscription,
    turn_active: Arc<AtomicBool>,
    /// Sub-agent progress mirrored by the session's event subscription.
    subagents: Arc<Mutex<Vec<SubagentInfo>>>,
    /// Latest plan submitted for review (PlanReady), for `plan_verdict`.
    pending_plan: Arc<Mutex<Option<PendingPlan>>>,
    /// Submissions parked while a turn runs; drained on `TurnFinished`.
    pending_submits: Arc<Mutex<Vec<QueuedSubmit>>>,
}

/// Per-surface command/event state. One instance per host surface (the
/// headless actor thread, or one per WebUI connection); the threads it owns
/// are shared with the app through the ThreadStore's `live_threads` dedup.
pub struct ActorState {
    pub sessions: HashMap<String, SessionState>,
    pub cwd: PathBuf,
    /// Thread id the host UI is currently showing. A turn that ends while its
    /// thread is unfocused marks itself unread so the list can badge it.
    pub focused: Arc<Mutex<Option<String>>>,
    /// Keeps the `ThreadStoreEvent` subscription alive for the state's
    /// lifetime; established on the first `list_threads`.
    pub store_subscription: Option<Subscription>,
    /// Memoized git-repository identity per path, so the workspace filter
    /// resolves each distinct project directory at most once.
    pub repo_ids: Arc<Mutex<HashMap<PathBuf, Option<PathBuf>>>>,
    /// In-flight stateless model completions keyed by request id; cancel
    /// signals the matching provider stream.
    pub model_chats: Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
}

impl ActorState {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            cwd,
            focused: Arc::new(Mutex::new(None)),
            store_subscription: None,
            repo_ids: Arc::new(Mutex::new(HashMap::new())),
            model_chats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn any_turn_active(&self) -> bool {
        self.sessions
            .values()
            .any(|s| s.turn_active.load(Ordering::SeqCst))
    }
}

/// Archive a session and dispose it — the single archive path shared by the
/// `/exit`/`/new` submit branch and the `archive_thread` command. An
/// in-flight turn is cancelled so the release is unconditional; the pi
/// transcript and sidecar persist, so reopening re-materializes the thread.
fn archive_and_dispose_session(state: &mut ActorState, id: &str, app: &mut App, sink: &EventSink) {
    ensure_store_subscription(app, state, sink);
    if let Some(session) = state.sessions.get(id)
        && session.turn_active.load(Ordering::SeqCst)
    {
        session.thread.update(app, |t, cx| t.cancel(cx));
    }
    // The store's centralized archive-teardown (archive_thread) cancels and
    // archives any live team led by the archived session; no explicit
    // disband here (an explicit one would bypass the is_leader gate and
    // blow up a whole team when a mere member row is archived).
    let store = agent::thread_store::global();
    store.update(app, |s, cx| {
        s.archive_thread(id, true, cx);
        // The disposal below drops the session's subscription, so the
        // backend's eventual `TurnFinished` can no longer clear the
        // store's live flags; reset them here or the archived row keeps
        // spinning until restart.
        s.mark_idle(id, cx);
        s.mark_pending_auth(id, false, cx);
        s.mark_pending_plan(id, false, cx);
        s.mark_background_work(id, false, cx);
    });
    {
        let mut focused = state.focused.lock().unwrap();
        if focused.as_deref() == Some(id) {
            *focused = None;
        }
    }
    state.sessions.remove(id);
    sink.emit(json!({"type": "session_disposed", "sessionId": id}).to_string());
}

/// Process one command against the shared thread entities. `state` is the
/// calling surface's own `ActorState`; the threads themselves are shared with
/// the app through the ThreadStore's live-thread dedup. Always returns
/// `true` — termination is the host's concern (the actor shell checks its
/// shutdown sentinel before calling, the WebUI bridge detaches instead).
pub fn handle_command(
    app: &mut App,
    state: &mut ActorState,
    sink: &EventSink,
    cmd: &Value,
) -> bool {
    let Some(cmd_name) = cmd["cmd"].as_str() else {
        return true;
    };
    let session_id = cmd["sessionId"].as_str().map(str::to_string);
    match cmd_name {
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
            let pending_plan = Arc::new(Mutex::new(None));
            let pending_submits = Arc::new(Mutex::new(Vec::new()));
            let thread = Thread::new_fresh(ThreadId(id.clone()), cwd, app);
            let subscription = subscribe_thread(
                app,
                &thread,
                id.clone(),
                turn_active.clone(),
                subagents.clone(),
                pending_plan.clone(),
                pending_submits.clone(),
                state.focused.clone(),
                sink.clone(),
            );
            if let Some(model) = agent::pi_providers::default_model() {
                thread.update(app, |t, cx| t.set_model(model, cx));
            }
            let persisted_mode = thread.read(app).permission_mode();
            state.sessions.insert(
                id.clone(),
                SessionState {
                    thread,
                    _subscription: subscription,
                    turn_active,
                    subagents,
                    pending_plan,
                    pending_submits,
                },
            );
            sink.emit(json!({"type": "session_created", "sessionId": id}).to_string());
            emit_persisted_permission_mode(persisted_mode, &id, sink);
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
                let persisted_mode = thread.read(app).permission_mode();
                emit_persisted_permission_mode(persisted_mode, &id, sink);
                emit_history_and_info(app, &thread, &id, &subagents, &session.pending_plan, sink);
                return true;
            }
            let thread = {
                let store = agent::thread_store::global();
                store.update(app, |s, app| s.load_thread(&id, app))
            };
            let Some(thread) = thread else {
                sink.emit(error_json(Some(&id), "thread not found"));
                return true;
            };
            {
                let store = agent::thread_store::global();
                store.update(app, |s, cx| s.set_unread(&id, false, cx));
            }
            let turn_active = Arc::new(AtomicBool::new(false));
            let subagents = Arc::new(Mutex::new(Vec::new()));
            let pending_plan = Arc::new(Mutex::new(None));
            let pending_submits = Arc::new(Mutex::new(Vec::new()));
            let subscription = subscribe_thread(
                app,
                &thread,
                id.clone(),
                turn_active.clone(),
                subagents.clone(),
                pending_plan.clone(),
                pending_submits.clone(),
                state.focused.clone(),
                sink.clone(),
            );
            let persisted_mode = thread.read(app).permission_mode();
            state.sessions.insert(
                id.clone(),
                SessionState {
                    thread,
                    _subscription: subscription,
                    turn_active,
                    subagents,
                    pending_submits,
                    pending_plan,
                },
            );
            sink.emit(json!({"type": "session_created", "sessionId": id}).to_string());
            emit_persisted_permission_mode(persisted_mode, &id, sink);
        }
        "focus_thread" => {
            *state.focused.lock().unwrap() = session_id.clone();
            if let Some(id) = session_id.as_deref()
                && state.store_subscription.is_some()
            {
                let id = id.to_string();
                let store = agent::thread_store::global();
                store.update(app, |s, cx| s.set_unread(&id, false, cx));
            }
        }
        "archive_thread" => {
            let Some(id) = session_id.clone() else {
                sink.emit(error_json(None, "archive_thread requires sessionId"));
                return true;
            };
            let archived = cmd["archived"].as_bool().unwrap_or(true);
            if archived {
                archive_and_dispose_session(state, &id, app, sink);
            } else {
                // Unarchive never touches sessions: an archived thread has no
                // live session, and content stays unloaded until explicitly
                // opened.
                ensure_store_subscription(app, state, sink);
                let store = agent::thread_store::global();
                store.update(app, |s, cx| s.archive_thread(&id, false, cx));
            }
        }
        "pin_thread" => {
            let Some(id) = session_id.clone() else {
                sink.emit(error_json(None, "pin_thread requires sessionId"));
                return true;
            };
            let pinned = cmd["pinned"].as_bool().unwrap_or(true);
            ensure_store_subscription(app, state, sink);
            let store = agent::thread_store::global();
            store.update(app, |s, cx| s.pin_thread(&id, pinned, cx));
        }
        "list_threads" => {
            ensure_store_subscription(app, state, sink);
            {
                let store = agent::thread_store::global();
                store.update(app, |s, cx| s.refresh(cx));
            }
            let threads = {
                let store = agent::thread_store::global();
                threads_snapshot(app, &store, &state.cwd, &state.repo_ids)
            };
            sink.emit(json!({"type": "threads_updated", "threads": threads}).to_string());
        }
        "list_commands" => {
            // The built-in set is shared with the gpui host via
            // `agent::slash_builtins`; descriptions ship as `i18n_key` and the
            // webview translates them in its own chrome locale. Ordering
            // mirrors the gpui popover: built-ins, then markdown macros, then
            // skills.
            let mut commands = Vec::new();
            for meta in agent::slash_builtins::BUILTIN_SLASH_COMMANDS {
                commands.push(json!({
                    "name": meta.name,
                    "description": null,
                    "kind": "command",
                    "argument_hint": null,
                    "i18n_key": meta.description_key,
                }));
            }
            let mut command_names: std::collections::HashSet<String> =
                std::collections::HashSet::from_iter(
                    agent::slash_builtins::BUILTIN_SLASH_COMMANDS
                        .iter()
                        .map(|meta| meta.name.to_string()),
                );
            if let Some(registry) = agent::command::try_global() {
                for (key, def) in registry.entries() {
                    // A macro sharing a built-in name is shadowed, matching
                    // the gpui registry's precedence.
                    if command_names.contains(key.as_str()) {
                        continue;
                    }
                    command_names.insert(key.clone());
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
                    if command_names.contains(key.as_str()) {
                        continue;
                    }
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
            emit_thread_info(app, &thread, &id, &subagents, sink);
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
                    session.thread.update(app, |t, cx| t.cancel(cx));
                    // The session's subscription is already dropped, so the
                    // backend's eventual `TurnFinished` can no longer clear
                    // the store's running flag; reset it here.
                    let store = agent::thread_store::global();
                    store.update(app, |s, cx| s.mark_idle(&id, cx));
                }
                sink.emit(
                    serde_json::json!({"type": "session_disposed", "sessionId": id}).to_string(),
                );
            }
        }
        "detach_session" => {
            let Some(id) = session_id.clone() else {
                return true;
            };
            // A browser refresh or closed tab must not kill a turn the app
            // may still be showing: drop the subscription and strong thread
            // reference without cancelling the thread or touching the store's
            // running flags. The live thread keeps streaming for the app
            // surface; a reconnect restores the bridge via `open_thread`.
            {
                let mut focused = state.focused.lock().unwrap();
                if focused.as_deref() == Some(id.as_str()) {
                    *focused = None;
                }
            }
            if state.sessions.remove(&id).is_some() {
                sink.emit(
                    serde_json::json!({"type": "session_disposed", "sessionId": id}).to_string(),
                );
            }
        }
        "submit" => {
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
            let slash: Option<(String, String)> = (images.is_empty())
                .then(|| parse_slash(&text))
                .flatten()
                .map(|(name, args)| (name.to_string(), args.to_string()));
            // Navigation built-ins (`/exit` / `/new` and aliases) are
            // session-level: cancel any in-flight turn, archive the thread,
            // and dispose the session so the webview returns to its home
            // composer, mirroring the gpui host's archive-and-fresh flow.
            // Takes effect immediately even while a turn is running.
            if let Some((name, _)) = slash.as_ref()
                && let Some(builtin) = agent::slash_builtins::canonical_builtin(name)
                && matches!(builtin.name, "exit" | "new")
            {
                let Some(id) = session_id.clone() else {
                    return true;
                };
                archive_and_dispose_session(state, &id, app, sink);
                return true;
            }
            with_session(state, session_id.as_deref(), sink, |session, _| {
                // A running turn parks plain (non-slash) submissions: the
                // webview echoes the bubble locally and the actor drains the
                // queue into one follow-up turn when the turn settles. Slash
                // turns keep executing immediately (their parameter forms
                // park the prompt in the thread's own queue via
                // `insert_slash_turn`, which the same drain carries out).
                if session.turn_active.load(Ordering::SeqCst) && slash.is_none() {
                    let client_id = cmd["clientId"].as_str().unwrap_or_default().to_string();
                    let ui = {
                        let t = session.thread.read(app);
                        MessageUiMetadata {
                            model_id: t.model().map(|m| m.id.clone()),
                            approval_mode: Some(t.permission_mode().as_i64()),
                            ..Default::default()
                        }
                    };
                    session.pending_submits.lock().unwrap().push(QueuedSubmit {
                        client_id,
                        text,
                        images,
                        ui,
                    });
                    return;
                }
                session.thread.update(app, |t, cx| {
                    let ui = MessageUiMetadata {
                        model_id: t.model().map(|m| m.id.clone()),
                        approval_mode: Some(t.permission_mode().as_i64()),
                        ..Default::default()
                    };
                    // Slash turns ride the registry (built-ins first,
                    // then markdown macros, then skills — the gpui
                    // registry's precedence); the bubble keeps the
                    // compact `/name args` form while the model sees the
                    // expanded body. Unmatched names fall through to a
                    // plain turn with the raw text.
                    if let Some((name, args)) = slash {
                        let slash_ui = MessageUiMetadata {
                            display_text: Some(text.clone()),
                            ..ui.clone()
                        };
                        let builtin_hit =
                            t.run_slash_builtin(&name, &args, Some(slash_ui.clone()), cx);
                        let command_hit = agent::command::try_global().is_some()
                            && t.submit_command(&name, &args, Some(slash_ui.clone()), cx);
                        let skill_hit = agent::skill::try_global().is_some()
                            && t.submit_skill(&name, &args, Some(slash_ui), cx);
                        if builtin_hit || command_hit || skill_hit {
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
        }
        "steer" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let client_id = cmd["clientId"].as_str().unwrap_or_default().to_string();
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
            // Remove the parked copy so the turn-end drain does not resend
            // the same message as a plain follow-up.
            session
                .pending_submits
                .lock()
                .unwrap()
                .retain(|q| q.client_id != client_id);
            let session_id = session_id.as_deref().unwrap_or_default();
            session.thread.update(app, |t, cx| {
                let ui = MessageUiMetadata {
                    model_id: t.model().map(|m| m.id.clone()),
                    approval_mode: Some(t.permission_mode().as_i64()),
                    ..Default::default()
                };
                let mut content = Vec::new();
                if !text.trim().is_empty() {
                    content.push(MessageContent::Text(text));
                }
                content.extend(
                    images
                        .into_iter()
                        .map(|(data, mime_type)| MessageContent::Image { data, mime_type }),
                );
                if t.is_running() {
                    let message_id = t.enqueue_steer(content, Some(ui), cx);
                    sink.emit(
                        serde_json::json!({
                            "type": "steer_pending",
                            "sessionId": session_id,
                            "clientId": client_id,
                            "messageId": message_id,
                        })
                        .to_string(),
                    );
                } else {
                    // Defensive: the webview only shows the steer action
                    // while a turn is active; an idle steer degrades to a
                    // regular message.
                    t.insert_user_message_with_content_and_ui_metadata(content, Some(ui), cx);
                    t.run_turn(cx);
                }
            });
        }),
        "drop_queued" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let client_id = cmd["clientId"].as_str().unwrap_or_default().to_string();
            session
                .pending_submits
                .lock()
                .unwrap()
                .retain(|q| q.client_id != client_id);
        }),
        "cancel_turn" => with_session(state, session_id.as_deref(), sink, |session, _| {
            session.thread.update(app, |t, cx| t.cancel(cx));
        }),
        "approve" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let id = cmd["id"].as_str().unwrap_or_default().to_string();
            let allow = cmd["allow"].as_bool().unwrap_or(false);
            let response = if allow {
                ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce)
            } else {
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny)
            };
            session
                .thread
                .update(app, |t, cx| t.respond_authorization(&id, response, cx));
        }),
        "answer_question" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let id = cmd["id"].as_str().unwrap_or_default().to_string();
            let answers: Vec<(String, String)> = cmd["answers"]
                .as_array()
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|pair| {
                            Some((
                                pair.get(0)?.as_str()?.to_string(),
                                pair.get(1)?.as_str().unwrap_or_default().to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let response = cmd["response"].as_str().map(str::to_string);
            session.thread.update(app, |t, cx| {
                t.respond_authorization(
                    &id,
                    ToolAuthorizationResponse::AskUserQuestion { answers, response },
                    cx,
                );
            });
        }),
        // Wire command name kept as `set_approval_mode` (host contract);
        // the mode value is the PermissionMode kebab wire name, unknown
        // values land on the bounded default.
        "set_approval_mode" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let mode = cmd["mode"]
                .as_str()
                .and_then(|raw| {
                    serde_json::from_value::<PermissionMode>(serde_json::Value::String(
                        raw.to_string(),
                    ))
                    .ok()
                })
                .unwrap_or_default();
            session
                .thread
                .update(app, |t, cx| t.set_permission_mode(mode, cx));
        }),
        "set_plan_mode" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let enabled = cmd["enabled"].as_bool().unwrap_or(false);
            session
                .thread
                .update(app, |t, cx| t.set_plan_mode(enabled, cx));
        }),
        "plan_verdict" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let choice = cmd["choice"].as_str().unwrap_or_default();
            // Consume the pending review on every verdict (mirrors the gpui
            // host's `take`): refine keeps plan mode on, but a later verdict
            // without a fresh ProposePlan must not seed execution again.
            let pending = session.pending_plan.lock().unwrap().take();
            let Some(pending) = pending else {
                sink.emit(error_json(session_id.as_deref(), "no pending plan review"));
                return;
            };
            session.thread.update(app, |t, cx| {
                t.set_plan_review_pending(false, cx);
                if choice == "refine" {
                    return;
                }
                let compact = choice == "execute_compact";
                let lang = t.agent_language();
                let seed_text = match agent::collaboration_mode::render_plan_mode_approved(
                    lang,
                    &pending.plan_file,
                ) {
                    Ok(text) => text,
                    Err(e) => {
                        cx.emit(agent::ThreadEvent::Error(e));
                        return;
                    }
                };
                let compact_instructions = compact.then(|| {
                    agent::collaboration_mode::plan_compact_instructions(lang, &pending.plan_file)
                });
                let ui = MessageUiMetadata {
                    model_id: t.model().map(|m| m.id.clone()),
                    approval_mode: Some(t.permission_mode().as_i64()),
                    author: Some(t.self_author()),
                    ..Default::default()
                };
                t.approve_plan(compact, compact_instructions, seed_text, Some(ui), cx);
            });
        }),
        "plan_seed_execution" => with_session(state, session_id.as_deref(), sink, |session, _| {
            let Some(plan_file) = cmd["planFile"].as_str() else {
                return;
            };
            let plan_file = plan_file.to_string();
            session.thread.update(app, |t, cx| {
                let ui = MessageUiMetadata {
                    model_id: t.model().map(|m| m.id.clone()),
                    approval_mode: Some(t.permission_mode().as_i64()),
                    author: Some(t.self_author()),
                    ..Default::default()
                };
                let lang = t.agent_language();
                // Fail-closed like `plan_verdict`: seeding execution with
                // an empty plan context is never a silent fallback.
                let seed_text =
                    match agent::collaboration_mode::render_plan_mode_approved(lang, &plan_file) {
                        Ok(text) => text,
                        Err(e) => {
                            cx.emit(agent::ThreadEvent::Error(e));
                            return;
                        }
                    };
                t.seed_plan_execution(plan_file, seed_text, Some(ui), cx);
            });
        }),
        "goal" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let action = cmd["action"].as_str().unwrap_or_default();
            let objective = cmd["objective"].as_str().unwrap_or_default().to_string();
            let budget = cmd["budget"].as_u64();
            let max_rounds = cmd["maxRounds"].as_u64();
            let actor = agent::db::GoalActor::User;
            let result = session.thread.update(app, |t, cx| match action {
                "create" => t.set_goal(objective, cx),
                "edit" => t.edit_goal(objective, budget, max_rounds, actor, cx),
                "replace" => t.replace_goal(objective, budget, max_rounds, actor, cx),
                "clear" => t.clear_goal(actor, cx),
                "pause" => t.set_goal_status(
                    agent::goal::GoalStatus::Paused,
                    Some(agent::goal::GoalBlockReason {
                        code: "user-paused".into(),
                        message: "paused by user".into(),
                    }),
                    actor,
                    cx,
                ),
                "resume" => t.set_goal_status(agent::goal::GoalStatus::Active, None, actor, cx),
                _ => Ok(()),
            });
            if let Err(e) = result {
                sink.emit(error_json(session_id.as_deref(), &e.to_string()));
            }
        }),
        "stop_background_task" => with_session(state, session_id.as_deref(), sink, |_, _| {
            let Some(task_id) = cmd["taskId"].as_str() else {
                return;
            };
            let task_id = task_id.to_string();
            agent::runtime::handle().spawn(async move {
                let _ = agent::background_task::stop(&task_id).await;
            });
        }),
        "set_model" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let Some(id) = cmd["id"].as_str() else {
                return;
            };
            let registry = agent::pi_providers::global();
            match pi_extensions::model_ref::resolve_model_ref(&registry, id) {
                Some(model) => {
                    session.thread.update(app, |t, cx| t.set_model(model, cx));
                }
                None => sink.emit(error_json(session_id.as_deref(), "unknown model")),
            }
        }),
        "set_reasoning_effort" => {
            with_session(state, session_id.as_deref(), sink, |session, sink| {
                let effort = match cmd["effort"].as_str() {
                    Some("high") => agent::language_model::ReasoningEffort::High,
                    Some("max") => agent::language_model::ReasoningEffort::Max,
                    _ => {
                        sink.emit(error_json(
                            session_id.as_deref(),
                            "set_reasoning_effort requires effort: high|max",
                        ));
                        return;
                    }
                };
                session
                    .thread
                    .update(app, |t, cx| t.set_reasoning_effort(effort, cx));
            })
        }
        "get_current_model" => with_session(state, session_id.as_deref(), sink, |session, sink| {
            let model = session.thread.read(app).model().cloned();
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
            let (usage, cost) = {
                let t = session.thread.read(app);
                (t.cumulative_token_usage(), t.cumulative_cost())
            };
            let json = serde_json::json!({
                "type": "usage",
                "sessionId": session_id,
                "usage": usage,
                "cost": cost,
            });
            sink.emit(json.to_string());
        }),
        "list_models" => {
            // Registration runs on a background thread (keychain/shell); a
            // reply must not race it with an empty snapshot — surfaces that
            // list before the build lands would never retry. Reuse the shared
            // push so the answer lands only once registration completes.
            spawn_models_push(sink.clone());
        }
        "model_chat" => {
            let Some(request_id) = cmd["requestId"].as_str().map(str::to_string) else {
                return true;
            };
            let Some(model_id) = cmd["model"].as_str() else {
                sink.emit(model_chat_done_error(
                    &request_id,
                    "model_chat requires a model id",
                ));
                return true;
            };
            let registry = agent::pi_providers::global();
            let Some(model) = pi_extensions::model_ref::resolve_model_ref(&registry, model_id)
            else {
                sink.emit(model_chat_done_error(&request_id, "unknown model"));
                return true;
            };
            let stream = match registry.resolve_stream(&model) {
                Ok(stream) => stream,
                Err(err) => {
                    sink.emit(model_chat_done_error(&request_id, &err.to_string()));
                    return true;
                }
            };
            let ctx = crate::model_chat::build_context(&model, &cmd["messages"], &cmd["tools"]);
            crate::model_chat::start(
                request_id,
                stream,
                ctx,
                sink.clone(),
                state.model_chats.clone(),
            );
        }
        "cancel_model_chat" => {
            if let Some(request_id) = cmd["requestId"].as_str() {
                crate::model_chat::cancel(&state.model_chats, request_id);
            }
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

/// Restored threads keep their persisted mode; replay it on open so the
/// surface renders the right permission toggle without a round trip. Wire
/// type kept as `approval_mode_changed` (host contract).
fn emit_persisted_permission_mode(mode: PermissionMode, session_id: &str, sink: &EventSink) {
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
/// bookkeeping (running / unread / pending-auth / pending-plan / errored /
/// background-work), aggregates sub-agent progress for the info panel, and
/// answers `HistoryRestored` with a full history snapshot.
#[allow(clippy::too_many_arguments)] // subscription setup: each input is a distinct owner/handle
fn subscribe_thread(
    app: &mut App,
    thread: &Entity<Thread>,
    session_id: String,
    turn_active: Arc<AtomicBool>,
    subagents: Arc<Mutex<Vec<SubagentInfo>>>,
    pending_plan: Arc<Mutex<Option<PendingPlan>>>,
    pending_submits: Arc<Mutex<Vec<QueuedSubmit>>>,
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
                ThreadEvent::TurnFinished {
                    cancelled, failed, ..
                } => {
                    turn_active.store(false, Ordering::SeqCst);
                    let unread = focused.lock().unwrap().as_deref() != Some(session_id.as_str());
                    let id = session_id.clone();
                    agent::thread_store::global().update(app, |s, cx| {
                        s.mark_idle(&id, cx);
                        s.mark_pending_auth(&id, false, cx);
                        s.mark_pending_plan(&id, false, cx);
                        // A successful finish supersedes a mid-turn error flag:
                        // the loop can recover (auto-retry, compact-and-retry)
                        // without a `TurnStarted` in between, so only
                        // `TurnStarted` and `TurnFinished` may clear it —
                        // mirroring the gpui host's `TurnFinished` handler.
                        if !*failed {
                            s.set_errored(&id, false, cx);
                        }
                        if unread {
                            s.set_unread(&id, true, cx);
                        }
                    });
                    // A settled (non-cancelled) run drains the parked
                    // submissions into one follow-up turn. The follow-up
                    // starts deferred so the current turn's `turn_finished`
                    // wire event lands before the re-entrant
                    // `turn_started` — the webview keys turnActive off
                    // those events, and the gpui host gets the same order
                    // by flushing last.
                    if !cancelled {
                        let drained = pending_submits
                            .lock()
                            .unwrap()
                            .drain(..)
                            .collect::<Vec<_>>();
                        let drained_any = !drained.is_empty();
                        if drained_any {
                            entity.update(app, |t, cx| {
                                for q in drained {
                                    let mut content = Vec::new();
                                    if !q.text.trim().is_empty() {
                                        content.push(MessageContent::Text(q.text));
                                    }
                                    content.extend(q.images.into_iter().map(
                                        |(data, mime_type)| MessageContent::Image {
                                            data,
                                            mime_type,
                                        },
                                    ));
                                    t.insert_user_message_with_content_and_ui_metadata(
                                        content,
                                        Some(q.ui),
                                        cx,
                                    );
                                }
                            });
                        }
                        let follow_up = entity.clone();
                        app.defer(move |app| {
                            follow_up.update(app, |t, cx| {
                                if drained_any || t.has_pending_prompts() {
                                    t.run_turn(cx);
                                }
                            });
                        });
                    }
                }
                ThreadEvent::ToolCallAuthorization { .. } => {
                    let id = session_id.clone();
                    agent::thread_store::global()
                        .update(app, |s, cx| s.mark_pending_auth(&id, true, cx));
                }
                ThreadEvent::Error(_) => {
                    let id = session_id.clone();
                    agent::thread_store::global().update(app, |s, cx| {
                        s.set_errored(&id, true, cx);
                        s.mark_pending_plan(&id, false, cx);
                        s.mark_background_work(&id, false, cx);
                    });
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
                    subagent_type,
                    tool_uses,
                    latest_activity,
                    status,
                    ..
                } => {
                    let mut list = subagents.lock().unwrap();
                    if let Some(entry) = list.iter_mut().find(|a| &a.id == id) {
                        entry.tool_uses = *tool_uses;
                        entry.latest_activity = latest_activity.clone();
                        entry.status = serde_json::to_value(status).unwrap_or(Value::Null);
                    } else {
                        // The pi backend emits no SubagentStarted, so the
                        // first progress sighting creates the row.
                        list.push(SubagentInfo {
                            id: id.clone(),
                            agent_type: subagent_type.clone(),
                            description: String::new(),
                            tool_uses: *tool_uses,
                            latest_activity: latest_activity.clone(),
                            status: serde_json::to_value(status).unwrap_or(Value::Null),
                        });
                    }
                }
                ThreadEvent::HistoryRestored => {
                    emit_history_and_info(
                        app,
                        &entity,
                        &session_id,
                        &subagents,
                        &pending_plan,
                        &sink,
                    );
                }
                ThreadEvent::GoalChanged { .. } => {
                    // The wire event carries the full snapshot (the pure
                    // projection only knows the active flag), mirroring the
                    // HistoryRestored pairing with `thread_history`.
                    let snapshot = entity.read(app).goal();
                    let id = session_id.clone();
                    sink.emit(
                        json!({
                            "type": "goal_changed",
                            "sessionId": id,
                            "snapshot": serde_json::to_value(snapshot).unwrap_or(Value::Null),
                        })
                        .to_string(),
                    );
                }
                ThreadEvent::PlanReady { plan_file, title } => {
                    // Record the review card identity so a later
                    // `plan_verdict` can seed execution without a round-trip,
                    // and enrich the wire event with the plan body for the
                    // review card. The projection also emits a bare
                    // `plan_ready`; the enriched one carries `content`.
                    *pending_plan.lock().unwrap() = Some(PendingPlan {
                        plan_file: plan_file.clone(),
                    });
                    // Mirror the gpui host's sidebar bookkeeping: the thread
                    // row shows the blue-static wait until the verdict lands.
                    let id = session_id.clone();
                    agent::thread_store::global()
                        .update(app, |s, cx| s.mark_pending_plan(&id, true, cx));
                    // Persist the pending verdict (sidecar) so a restarted
                    // session re-surfaces the card — the engine re-emits
                    // `PlanReady` on Ready only when this flag is recorded.
                    // The gpui host mirrors the same call on its review card;
                    // without it a session torn down before the verdict
                    // (webview/window reload, chat-participant handoff) is
                    // left in plan mode with no review card to resolve.
                    entity.update(app, |t, cx| t.set_plan_review_pending(true, cx));
                    let content = std::fs::read_to_string(plan_file).unwrap_or_default();
                    let id = session_id.clone();
                    sink.emit(
                        json!({
                            "type": "plan_ready",
                            "sessionId": id,
                            "plan_file": plan_file,
                            "title": title,
                            "content": content,
                        })
                        .to_string(),
                    );
                }
                ThreadEvent::BackgroundTaskUpdated { .. } => {
                    // Live monitors / background bash keep the loop able to
                    // self-advance; mirror the gpui host's per-thread
                    // running-task check so the row keeps spinning even with
                    // no turn in flight.
                    let id = session_id.clone();
                    agent::thread_store::global().update(app, |s, cx| {
                        s.mark_background_work(
                            &id,
                            agent::background_task::thread_has_running_tasks(&id),
                            cx,
                        );
                    });
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
    pending_plan: &Mutex<Option<PendingPlan>>,
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
    // Re-emit in-flight interaction cards a reloaded webview lost: pending
    // questions re-fold as the ask card and a submitted plan re-folds as
    // the review card. The webview store folds these as upserts, so a
    // session that never lost state stays single-card. Every interaction
    // surfaces as `AskUserQuestion` live — replay the same shape so the
    // card renders identically after a reload.
    for (id, meta) in thread.read(app).pending_auth_entries() {
        sink.emit(
            json!({
                "type": "tool_call_authorization",
                "sessionId": session_id,
                "id": id,
                "tool_name": agent::tools::ASK_USER_QUESTION,
                "summary": meta.summary,
                "input": meta.input,
            })
            .to_string(),
        );
    }
    if let Some(pending) = pending_plan.lock().unwrap().as_ref() {
        let content = std::fs::read_to_string(&pending.plan_file).unwrap_or_default();
        let slug = std::path::Path::new(&pending.plan_file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .trim_end_matches("-plan")
            .to_string();
        let title = agent::plan_mode::resolve_plan_title(None, &content, &slug);
        sink.emit(
            json!({
                "type": "plan_ready",
                "sessionId": session_id,
                "plan_file": pending.plan_file,
                "title": title,
                "content": content,
            })
            .to_string(),
        );
    }
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
                "reasoning_effort": t.reasoning_effort().wire_value(),
                "worktree_path": worktree_path,
                "plan": t.persisted_plan().and_then(|p| serde_json::to_value(p).ok()),
                "goal": serde_json::to_value(t.goal()).unwrap_or(Value::Null),
                "usage": t.cumulative_token_usage(),
                "per_model_usage": t.per_model_token_usage(),
                "per_model_last_usage": t.per_model_last_request_usage(),
                "per_model_cost": t.per_model_cost(),
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

/// `threads_updated` item list, scoped to the workspace's live sessions:
/// the workspace directory itself plus every worktree of the same git
/// repository.
fn threads_snapshot(
    app: &App,
    store: &Entity<ThreadStore>,
    cwd: &Path,
    repo_ids: &Mutex<HashMap<PathBuf, Option<PathBuf>>>,
) -> Value {
    let store = store.read(app);
    let mut cache = repo_ids.lock().unwrap();
    let rows = store
        .summaries()
        .iter()
        .chain(store.archived_summaries())
        // Archived rows stay in the snapshot so the surface can render them
        // behind its "more" affordance instead of dropping them.
        .filter(|s| matches_workspace(&s.project, cwd, &mut cache))
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.display_title(),
                "updated_at": s.interacted_at,
                "running": store.is_running(&s.id),
                "unread": s.has_unread,
                "errored": s.errored,
                "pending_auth": store.pending_auth_contains(&s.id),
                "pending_plan": store.pending_plan_contains(&s.id),
                "background_work": store.background_work_contains(&s.id),
                "model_id": s.model_id,
                "pinned": s.pinned,
                "archived": s.archived,
                "parent_id": s.parent_id,
                "depth": s.depth,
            })
        })
        .collect();
    Value::Array(rows)
}

/// Whether a session's `project` directory belongs to the workspace rooted
/// at `cwd`: the same path, or a worktree of the same git repository.
fn matches_workspace(
    project: &str,
    cwd: &Path,
    cache: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> bool {
    if project == cwd.to_string_lossy().as_ref() {
        return true;
    }
    let Some(cwd_id) = repo_identity_cached(cwd, cache) else {
        return false;
    };
    repo_identity_cached(&PathBuf::from(project), cache).is_some_and(|id| id == cwd_id)
}

fn repo_identity_cached(
    path: &Path,
    cache: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> Option<PathBuf> {
    if let Some(identity @ Some(_)) = cache.get(path) {
        // A confirmed repository identity is stable for the actor's
        // lifetime.
        return identity.clone();
    }
    // A miss may just predate a `git init` in the workspace, so it is
    // rechecked on every call instead of caching the negative.
    let identity = repo_identity(path);
    if identity.is_some() {
        cache.insert(path.to_path_buf(), identity.clone());
    }
    identity
}

/// The canonical git common directory of the repository owning `path` —
/// shared by every worktree of that repository — or `None` outside git.
fn repo_identity(path: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dir.is_empty() {
        return None;
    }
    // Relative output resolves against the queried directory; an absolute
    // common dir (linked worktrees) replaces the base outright.
    let joined = path.join(dir);
    Some(std::fs::canonicalize(&joined).unwrap_or(joined))
}

/// Subscribe once to store updates so list mutations (title generation,
/// unread sidecars, running flips) push fresh snapshots without polling.
fn ensure_store_subscription(app: &mut App, state: &mut ActorState, sink: &EventSink) {
    if state.store_subscription.is_some() {
        return;
    }
    let cwd = state.cwd.clone();
    let repo_ids = state.repo_ids.clone();
    let sink = sink.clone();
    let store = agent::thread_store::global();
    state.store_subscription = Some(app.subscribe(
        &store,
        move |store: Entity<ThreadStore>, _ev: &ThreadStoreEvent, app: &mut App| {
            let threads = threads_snapshot(app, &store, &cwd, &repo_ids);
            sink.emit(json!({"type": "threads_updated", "threads": threads}).to_string());
        },
    ));
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
/// Leading whitespace is tolerated, matching the gpui host's `parse`.
fn parse_slash(text: &str) -> Option<(&str, &str)> {
    let body = text.trim_start().strip_prefix('/')?;
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
        "provider_name": agent::pi_providers::display_provider_name(model),
        "api": model.api,
        "context_window": model.context_window,
        "max_tokens": model.max_tokens,
    })
}

/// Error settlement for a `model_chat` that never started streaming (missing
/// model id, unknown model, unresolvable provider runtime).
fn model_chat_done_error(request_id: &str, error: &str) -> String {
    json!({
        "type": "model_chat_done",
        "requestId": request_id,
        "stop": null,
        "error": error,
    })
    .to_string()
}

/// Collapse models sharing one registration identity: same provider
/// registration name (which is unique per wire endpoint, `{name}-{wire}`) and
/// model id. Distinct wire-api variants of one provider stay separate — the
/// VS Code native-chat registration constraint that used to live here moved
/// to the extension side (per-bucket id dedup in `ManoxModelProvider`).
fn deduped_models(models: Vec<pi::types::Model>) -> Vec<pi::types::Model> {
    let mut seen = HashSet::new();
    models
        .into_iter()
        .filter(|model| seen.insert((model.provider.clone(), model.id.clone())))
        .collect()
}

fn models_snapshot() -> Value {
    let models: Vec<Value> = deduped_models(agent::pi_providers::global().models())
        .iter()
        .map(model_json)
        .collect();
    json!({ "type": "models", "models": models })
}

/// Push a `models` snapshot once the one-shot provider registration
/// completes: surfaces that listed before then saw an empty registry and
/// never retried, leaving the model picker permanently disabled. Public so
/// the actor shell can kick it off from its `init` branch.
pub fn spawn_models_push(sink: EventSink) {
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
    use std::thread;
    use std::time::Duration;

    use gpui::HeadlessAppContext;

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

    /// Parse and run a command on the context's `App`, mirroring how the
    /// host shell delegates into `handle_command`. Returns `()`; settlement
    /// happens on the following `run_until_parked`.
    fn cmd(cx: &mut HeadlessAppContext, state: &mut ActorState, sink: &EventSink, command: &str) {
        let value: Value = serde_json::from_str(command).expect("test command JSON");
        cx.update(|app| handle_command(app, state, sink, &value));
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
            repo_ids: Arc::new(Mutex::new(HashMap::new())),
            model_chats: Arc::new(Mutex::new(HashMap::new())),
        };
        let (out, sink) = collect_sink();

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        assert!(state.sessions.contains_key("s1"));
        assert!(types(&out).contains(&"session_created".to_string()));

        // A fresh session opens focused and replays its permission mode, so
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
        assert_eq!(modes, vec!["workspace-write".to_string()]);

        // Switching the permission mode surfaces as an approval_mode_changed
        // event for the session (wire type kept for the host contract).
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"set_approval_mode","sessionId":"s1","mode":"danger-full-access"}"#,
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
        assert_eq!(
            modes,
            vec![
                "workspace-write".to_string(),
                "danger-full-access".to_string()
            ]
        );

        // A command for an unknown session surfaces as an error event.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"nope","text":"hi"}"#,
        );
        assert!(types(&out).contains(&"error".to_string()));

        cmd(
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

    /// Archiving a live session releases it: the row moves to the archived
    /// partition, the session is disposed, and `session_disposed` lands so
    /// the webview folds the thread state away.
    #[test]
    fn archive_thread_disposes_live_session() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/archive/dispose/project");
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/archive/dispose/project"));
        let (out, sink) = collect_sink();

        let rows = |out: &Arc<Mutex<Vec<String>>>, cx: &mut HeadlessAppContext| -> Vec<Value> {
            cx.run_until_parked();
            out.lock()
                .unwrap()
                .iter()
                .rev()
                .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                .find(|e| e["type"] == "threads_updated")
                .and_then(|s| s["threads"].as_array().cloned())
                .unwrap_or_default()
        };
        let wait_for = |out: &Arc<Mutex<Vec<String>>>,
                        cx: &mut HeadlessAppContext,
                        check: &dyn Fn(&[Value]) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if check(&rows(out, cx)) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "snapshot never settled"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        wait_for(&out, &mut cx, &|r| r.iter().any(|t| t["id"] == "s1"));
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"open_thread","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        assert!(state.sessions.contains_key("s1"));

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"archive_thread","sessionId":"s1","archived":true}"#,
        );
        cx.run_until_parked();
        assert!(!state.sessions.contains_key("s1"));
        assert!(types(&out).contains(&"session_disposed".to_string()));
        wait_for(&out, &mut cx, &|r| {
            r.iter()
                .find(|t| t["id"] == "s1")
                .is_some_and(|t| t["archived"].as_bool() == Some(true))
        });

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// Archiving a thread with an in-flight turn cancels the turn and still
    /// releases the session — the archive path is unconditional.
    #[test]
    fn archive_thread_cancels_running_turn() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/"));
        let (out, sink) = collect_sink();

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        state
            .sessions
            .get_mut("s1")
            .expect("session live")
            .turn_active
            .store(true, Ordering::SeqCst);

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"archive_thread","sessionId":"s1","archived":true}"#,
        );
        cx.run_until_parked();
        assert!(!state.sessions.contains_key("s1"));
        assert!(types(&out).contains(&"session_disposed".to_string()));

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// Unarchiving only flips the list flag — it never re-creates a session
    /// and never emits another `session_disposed`.
    #[test]
    fn unarchive_does_not_dispose_or_load() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/archive/dispose/project");
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/archive/dispose/project"));
        let (out, sink) = collect_sink();

        let rows = |out: &Arc<Mutex<Vec<String>>>, cx: &mut HeadlessAppContext| -> Vec<Value> {
            cx.run_until_parked();
            out.lock()
                .unwrap()
                .iter()
                .rev()
                .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                .find(|e| e["type"] == "threads_updated")
                .and_then(|s| s["threads"].as_array().cloned())
                .unwrap_or_default()
        };
        let wait_for = |out: &Arc<Mutex<Vec<String>>>,
                        cx: &mut HeadlessAppContext,
                        check: &dyn Fn(&[Value]) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if check(&rows(out, cx)) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "snapshot never settled"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        wait_for(&out, &mut cx, &|r| r.iter().any(|t| t["id"] == "s1"));
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"open_thread","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"archive_thread","sessionId":"s1","archived":true}"#,
        );
        cx.run_until_parked();
        assert!(!state.sessions.contains_key("s1"));
        let disposed_before = types(&out)
            .iter()
            .filter(|t| t.as_str() == "session_disposed")
            .count();

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"archive_thread","sessionId":"s1","archived":false}"#,
        );
        cx.run_until_parked();
        let disposed_after = types(&out)
            .iter()
            .filter(|t| t.as_str() == "session_disposed")
            .count();
        assert_eq!(disposed_after, disposed_before);
        assert!(!state.sessions.contains_key("s1"));
        wait_for(&out, &mut cx, &|r| {
            r.iter()
                .find(|t| t["id"] == "s1")
                .is_some_and(|t| t["archived"].as_bool() == Some(false))
        });

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn set_reasoning_effort_routes_and_rejects_invalid() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        let mut state = state_with(PathBuf::from("/"));
        let (out, sink) = collect_sink();

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"set_reasoning_effort","sessionId":"s1","effort":"max"}"#,
        );
        cx.run_until_parked();
        let events: Vec<String> = out
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_str::<serde_json::Value>(raw).unwrap())
            .filter(|v| v["type"] == "reasoning_effort_changed")
            .map(|v| v["effort"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(events, vec!["max".to_string()]);
        let thread_effort = cx.update(|app| {
            state
                .sessions
                .get("s1")
                .map(|s| s.thread.read(app).reasoning_effort())
        });
        assert_eq!(
            thread_effort,
            Some(agent::language_model::ReasoningEffort::Max)
        );

        // An unparseable effort surfaces as an error event and leaves the
        // thread's effort untouched.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"set_reasoning_effort","sessionId":"s1","effort":"turbo"}"#,
        );
        let errors: Vec<String> = out
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_str::<serde_json::Value>(raw).unwrap())
            .filter(|v| v["type"] == "error")
            .map(|v| v["message"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            errors
                .iter()
                .any(|m| m.contains("set_reasoning_effort requires effort")),
            "{errors:?}"
        );

        drop(state);
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
            repo_ids: Arc::new(Mutex::new(HashMap::new())),
            model_chats: Arc::new(Mutex::new(HashMap::new())),
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

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
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

    /// Session files the store scan picks up: one header line per session.
    fn seed_session_file(dir: &Path, id: &str, cwd: &str) {
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-05-28T07:13:46.608Z\",\"cwd\":\"{cwd}\"}}\n"
            ),
        )
        .unwrap();
    }

    /// Seed a session file whose header records a team parent link.
    fn seed_session_file_team(dir: &Path, id: &str, cwd: &str, parent: &str) {
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-05-28T07:13:46.608Z\",\"cwd\":\"{cwd}\",\"metadata\":{{\"host\":\"manox\",\"team\":{{\"parent\":\"{parent}\"}}}}}}\n"
            ),
        )
        .unwrap();
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A main checkout, a linked worktree of it, and an unrelated repo.
    /// Idempotent: the fixture survives across tests in the same process.
    fn git_worktree_fixture() -> (PathBuf, PathBuf, PathBuf) {
        let fixtures = PathBuf::from(std::env::var("HOME").unwrap()).join("worktree-fixtures");
        let main = fixtures.join("main");
        let wt = fixtures.join("wt");
        let other = fixtures.join("other");
        if main.join(".git").exists() {
            return (main, wt, other);
        }
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        run_git(&main, &["init"]);
        run_git(
            &main,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        run_git(&main, &["worktree", "add", wt.to_str().unwrap()]);
        run_git(&other, &["init"]);
        (main, wt, other)
    }

    #[test]
    fn worktree_paths_share_one_repo_identity() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let (main, wt, other) = git_worktree_fixture();
        let mut cache = HashMap::new();
        assert!(matches_workspace(wt.to_str().unwrap(), &main, &mut cache));
        assert!(matches_workspace(main.to_str().unwrap(), &wt, &mut cache));
        assert!(!matches_workspace(
            other.to_str().unwrap(),
            &main,
            &mut cache
        ));
        // A directory outside git matches only itself.
        let bare = main.parent().unwrap().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(matches_workspace(bare.to_str().unwrap(), &bare, &mut cache));
        assert!(!matches_workspace(
            bare.to_str().unwrap(),
            &main,
            &mut cache
        ));
        // A non-git workspace only ever matches its exact full path: a
        // subdirectory of the workspace is not the workspace.
        let child = bare.join("child");
        std::fs::create_dir_all(&child).unwrap();
        assert!(!matches_workspace(
            child.to_str().unwrap(),
            &bare,
            &mut cache
        ));
        assert!(!matches_workspace(
            bare.to_str().unwrap(),
            &child,
            &mut cache
        ));
    }

    /// A non-git path may become a repository while the actor lives (the
    /// user runs `git init` in the workspace), so a miss must not be cached
    /// forever.
    #[test]
    fn repo_identity_rechecks_a_formerly_non_git_path() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let dir = PathBuf::from(std::env::var("HOME").unwrap()).join("late-init");
        std::fs::create_dir_all(&dir).unwrap();
        let mut cache = HashMap::new();
        assert_eq!(repo_identity_cached(&dir, &mut cache), None);
        run_git(&dir, &["init"]);
        let identity = repo_identity_cached(&dir, &mut cache);
        assert!(identity.is_some());
        // Once confirmed, the identity is served from the cache.
        assert_eq!(cache.get(&dir), Some(&identity));
    }

    #[test]
    fn list_threads_includes_worktrees_of_the_workspace_repo() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let (main, wt, other) = git_worktree_fixture();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "wt-main", main.to_str().unwrap());
        seed_session_file(&sessions, "wt-linked", wt.to_str().unwrap());
        seed_session_file(&sessions, "wt-other", other.to_str().unwrap());

        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(main.clone());
        let (out, sink) = collect_sink();

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);

        // The scan is async; wait for a snapshot that has absorbed the
        // seeded sessions.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let ids = loop {
            cx.run_until_parked();
            let snapshot = out
                .lock()
                .unwrap()
                .iter()
                .rev()
                .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                .find(|e| e["type"] == "threads_updated");
            let ids: Vec<String> = snapshot
                .and_then(|s| s["threads"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|t| t["id"].as_str().map(str::to_string))
                .collect();
            if ids.iter().any(|id| id == "wt-linked") {
                break ids;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worktree session never appeared in the snapshot"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(ids.contains(&"wt-main".to_string()));
        assert!(
            !ids.contains(&"wt-other".to_string()),
            "sessions from an unrelated repo must stay hidden"
        );

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn archive_and_pin_commands_flag_the_snapshot_rows() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "ap-plain", "/archive/pin/project");
        seed_session_file(&sessions, "ap-pin", "/archive/pin/project");
        seed_session_file(&sessions, "ap-archive", "/archive/pin/project");

        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/archive/pin/project"));
        let (out, sink) = collect_sink();

        let latest_rows = |cx: &mut HeadlessAppContext| -> Vec<Value> {
            cx.run_until_parked();
            out.lock()
                .unwrap()
                .iter()
                .rev()
                .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                .find(|e| e["type"] == "threads_updated")
                .and_then(|s| s["threads"].as_array().cloned())
                .unwrap_or_default()
        };

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        // The mutations resolve ids through the scan's path table, so they
        // must wait for the seeded sessions to land in a snapshot.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let ids: Vec<String> = latest_rows(&mut cx)
                .iter()
                .filter_map(|t| t["id"].as_str().map(str::to_string))
                .collect();
            if ids
                .iter()
                .all(|id| ["ap-plain", "ap-pin", "ap-archive"].contains(&id.as_str()))
                && ids.len() == 3
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "seeded sessions never appeared in the snapshot"
            );
            thread::sleep(Duration::from_millis(10));
        }

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"pin_thread","sessionId":"ap-pin","pinned":true}"#,
        );
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"archive_thread","sessionId":"ap-archive","archived":true}"#,
        );

        // The meta writes land asynchronously; the settled snapshot keeps
        // every row — archived flagged instead of dropped, pinned flagged
        // for the surface's sort, and plain rows explicitly unflagged.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let rows = latest_rows(&mut cx);
            let by_id = |id: &str| rows.iter().find(|t| t["id"] == id).cloned();
            let settled = by_id("ap-pin").is_some_and(|t| t["pinned"].as_bool() == Some(true))
                && by_id("ap-archive").is_some_and(|t| t["archived"].as_bool() == Some(true))
                && by_id("ap-plain").is_some_and(|t| {
                    t["pinned"].as_bool() == Some(false) && t["archived"].as_bool() == Some(false)
                });
            if settled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "archive/pin flags never settled in the snapshot; last rows: {rows:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// Live-state flags the gpui sidebar mirrors — `pending_plan` (blue-static
    /// wait for the review verdict) and `background_work` (row spins while
    /// monitors / background bash are alive) — must ride the same
    /// `threads_updated` rows so the VS Code surface renders the same state
    /// machine.
    #[test]
    fn threads_snapshot_carries_pending_plan_and_background_work() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "live-flags", "/live/flags/project");

        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/live/flags/project"));
        let (out, sink) = collect_sink();

        let latest_rows = |cx: &mut HeadlessAppContext| -> Vec<Value> {
            cx.run_until_parked();
            out.lock()
                .unwrap()
                .iter()
                .rev()
                .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                .find(|e| e["type"] == "threads_updated")
                .and_then(|s| s["threads"].as_array().cloned())
                .unwrap_or_default()
        };

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let ids: Vec<String> = latest_rows(&mut cx)
                .iter()
                .filter_map(|t| t["id"].as_str().map(str::to_string))
                .collect();
            if ids.iter().any(|id| id == "live-flags") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "seeded session never appeared in the snapshot"
            );
            thread::sleep(Duration::from_millis(10));
        }

        // Flag the store the way the actor's subscription does on PlanReady /
        // BackgroundTaskUpdated; each write emits a store event that pushes a
        // fresh snapshot through the subscription.
        cx.update(|app| {
            agent::thread_store::global().update(app, |s, cx| {
                s.mark_pending_plan("live-flags", true, cx);
                s.mark_background_work("live-flags", true, cx);
            });
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let row = latest_rows(&mut cx)
                .into_iter()
                .find(|t| t["id"] == "live-flags");
            let flagged = row.is_some_and(|t| {
                t["pending_plan"].as_bool() == Some(true)
                    && t["background_work"].as_bool() == Some(true)
            });
            if flagged {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "live-state flags never reached the snapshot"
            );
            thread::sleep(Duration::from_millis(10));
        }

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// A mid-turn stream error (provider hiccup) flags the row errored;
    /// the loop's auto-retry then completes the same turn without a
    /// `TurnStarted` in between, so only a successful `TurnFinished` can
    /// clear the flag — the webview's thread-row state must follow the
    /// turn's real outcome, mirroring the gpui host's `TurnFinished`
    /// handler.
    #[test]
    fn successful_finish_clears_stale_errored_flag() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/proj");

        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/proj"));
        let (out, sink) = collect_sink();

        let snapshot_rows =
            |out: &Arc<Mutex<Vec<String>>>, cx: &mut HeadlessAppContext| -> Vec<Value> {
                cx.run_until_parked();
                out.lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                    .find(|e| e["type"] == "threads_updated")
                    .and_then(|s| s["threads"].as_array().cloned())
                    .unwrap_or_default()
            };
        let row_errored =
            |out: &Arc<Mutex<Vec<String>>>, cx: &mut HeadlessAppContext| -> Option<bool> {
                snapshot_rows(out, cx)
                    .into_iter()
                    .find(|t| t["id"] == "s1")
                    .and_then(|t| t["errored"].as_bool())
            };
        let wait_for_errored = |out: &Arc<Mutex<Vec<String>>>,
                                cx: &mut HeadlessAppContext,
                                expected: bool,
                                msg: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let flag = row_errored(out, cx);
                if flag == Some(expected) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{msg}; last errored flag: {flag:?}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };

        // The seeded session appears in the list snapshot (the row whose
        // errored flag the webview renders).
        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if snapshot_rows(&out, &mut cx).iter().any(|t| t["id"] == "s1") {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "seeded session never appeared in the snapshot"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        // A live session drives the wire events that flip the row's flag.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1","cwd":"/proj"}"#,
        );
        cx.run_until_parked();
        let (engine, events) = FakeEngine::new();
        cx.update(|app| {
            state.sessions["s1"].thread.update(app, |t, cx| {
                t.set_engine_for_test(engine.clone(), events, cx);
            });
        });

        // The fresh thread's real engine cannot assemble a session in this
        // hermetic env (no providers registered) and bails with a Fatal
        // notice, which also flags the row errored. Drain it before the
        // assertions so the startup failure cannot race the finish below.
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if types(&out).contains(&"error".to_string()) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the engine's startup error never arrived; types: {:?}",
                    types(&out)
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
        wait_for_errored(
            &out,
            &mut cx,
            true,
            "the startup failure never flagged the row",
        );

        engine
            .notices
            .send(agent::thread_engine::BackendNotice::Event(Box::new(
                agent::ThreadEvent::Error(anyhow::anyhow!("transient provider failure")),
            )))
            .unwrap();
        wait_for_errored(&out, &mut cx, true, "the error never flagged the row");

        // The auto-retry completes the same turn (no `TurnStarted` in
        // between): the row must follow the real outcome.
        engine
            .notices
            .send(agent::thread_engine::BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        wait_for_errored(
            &out,
            &mut cx,
            false,
            "a successful finish never cleared the errored flag",
        );

        // A failed finish keeps the flag.
        engine
            .notices
            .send(agent::thread_engine::BackendNotice::Event(Box::new(
                agent::ThreadEvent::Error(anyhow::anyhow!("another failure")),
            )))
            .unwrap();
        wait_for_errored(
            &out,
            &mut cx,
            true,
            "the second error never flagged the row",
        );
        engine
            .notices
            .send(agent::thread_engine::BackendNotice::Settled {
                cancelled: false,
                failed: true,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            row_errored(&out, &mut cx),
            Some(true),
            "a failed finish must keep the errored flag"
        );

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// A team worker's session header carries its leader's id; the snapshot
    /// surfaces it as `parent_id` with a computed `depth` (1 under a
    /// top-level leader), so the sidebar can nest the member row.
    #[test]
    fn threads_snapshot_carries_team_parent_and_depth() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "leader-1", "/team/project");
        seed_session_file_team(&sessions, "member-1", "/team/project", "leader-1");

        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        cx.update(agent::thread_store::init);
        let mut state = state_with(PathBuf::from("/team/project"));
        let (out, sink) = collect_sink();

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let rows = loop {
            cx.run_until_parked();
            let rows = out
                .lock()
                .unwrap()
                .iter()
                .rev()
                .map(|raw| serde_json::from_str::<Value>(raw).unwrap())
                .find(|e| e["type"] == "threads_updated")
                .and_then(|s| s["threads"].as_array().cloned())
                .unwrap_or_default();
            let ids: Vec<&str> = rows.iter().filter_map(|t| t["id"].as_str()).collect();
            if ids.contains(&"member-1") {
                break rows;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "seeded team session never appeared in the snapshot"
            );
            thread::sleep(Duration::from_millis(10));
        };

        let member = rows.iter().find(|t| t["id"] == "member-1").unwrap();
        assert_eq!(member["parent_id"], "leader-1");
        assert_eq!(member["depth"], 1);
        let leader = rows.iter().find(|t| t["id"] == "leader-1").unwrap();
        assert!(leader["parent_id"].is_null());
        assert_eq!(leader["depth"], 0);

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// A session file with no `metadata.host` tag belongs to the native-app
    /// host; one tagged `vscode` belongs to the VS Code host. Switching the
    /// process host flips which of the two a `list_threads` snapshot shows.
    /// The host switch goes through `set_host` directly: a full `init`
    /// command would run `agent::init` (MCP/plugin/provider registration),
    /// whose background side effects destabilize sibling tests in the suite.
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

        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_commands"}"#);

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
        let commands = payload["commands"].as_array().expect("commands array");
        // The shared built-in set leads the list (the hermetic registries
        // still carry the compiled-in `healthz` markdown macro afterwards),
        // each entry with a null description and an i18n_key the webview
        // translates.
        let names: Vec<&str> = commands
            .iter()
            .map(|c| c["name"].as_str().expect("command name"))
            .collect();
        let expected: Vec<&str> = agent::slash_builtins::BUILTIN_SLASH_COMMANDS
            .iter()
            .map(|meta| meta.name)
            .collect();
        assert_eq!(&names[..expected.len()], expected.as_slice());
        for command in commands.iter().take(expected.len()) {
            assert_eq!(command["kind"], "command");
            assert!(command["description"].is_null());
            assert_eq!(
                command["i18n_key"],
                agent::slash_builtins::canonical_builtin(
                    command["name"].as_str().expect("command name"),
                )
                .expect("builtin metadata")
                .description_key
            );
        }
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

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"hello","images":[{"data":"aGVsbG8=","mimeType":"image/png"}]}"#,
        );

        let messages = cx.update(|app| state.sessions["s1"].thread.read(app).messages().to_vec());
        assert_eq!(messages.len(), 1);
        let ui = messages[0].ui.as_ref().expect("ui metadata attached");
        assert_eq!(ui.approval_mode, Some(PermissionMode::default().as_i64()));
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
        cmd(
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
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"dispose_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// Create one session (plus store + globals) and return the sink state
    /// borrowed for the test body.
    fn with_session_for_submit(
        cx: &mut HeadlessAppContext,
        state: &mut ActorState,
    ) -> Arc<Mutex<Vec<String>>> {
        init_globals(cx);
        cx.update(agent::thread_store::init);
        let (out, sink) = collect_sink();
        cmd(
            cx,
            state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        out
    }

    #[test]
    fn submit_routes_builtin_plan_and_mode() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink_out = out.clone();
        let sink = EventSink::new(move |json| sink_out.lock().unwrap().push(json));

        // `/plan <prompt>` enters plan mode and starts a planning turn whose
        // user message carries the compact display form.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"/plan fix the auth flow"}"#,
        );
        cx.run_until_parked();
        assert!(cx.update(|app| state.sessions["s1"].thread.read(app).plan_mode()));
        let messages = cx.update(|app| state.sessions["s1"].thread.read(app).messages().to_vec());
        let last = messages.last().expect("plan prompt inserted");
        assert!(
            last.content
                .iter()
                .any(|c| matches!(c, MessageContent::Text(t) if t == "fix the auth flow"))
        );
        assert_eq!(
            last.ui.as_ref().and_then(|ui| ui.display_text.as_deref()),
            Some("/plan fix the auth flow")
        );

        // Bare `/plan` toggles plan mode back off without starting a turn.
        // (The engine is absent in the hermetic test env — no provider — so
        // the async `plan_mode_changed` notice never lands here; its
        // projection is covered by the events module tests.)
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"/plan"}"#,
        );
        cx.run_until_parked();
        assert!(!cx.update(|app| state.sessions["s1"].thread.read(app).plan_mode()));
        // Still exactly one turn (from the prompt form above): the bare
        // toggle must not start another.
        assert_eq!(
            types(&out)
                .iter()
                .filter(|t| t.as_str() == "turn_started")
                .count(),
            1
        );

        // Bare `/mode` cycles the permission mode (default WorkspaceWrite
        // → DangerFullAccess) and pushes the change.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"/mode"}"#,
        );
        cx.run_until_parked();
        assert_eq!(
            cx.update(|app| state.sessions["s1"].thread.read(app).permission_mode()),
            PermissionMode::DangerFullAccess
        );
        assert!(types(&out).contains(&"approval_mode_changed".to_string()));

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn submit_routes_builtin_exit_and_goal() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink_out = out.clone();
        let sink = EventSink::new(move |json| sink_out.lock().unwrap().push(json));

        // `/exit` archives the thread and disposes the session so the
        // webview returns to its home composer.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"/exit"}"#,
        );
        cx.run_until_parked();
        assert!(!state.sessions.contains_key("s1"));
        assert!(types(&out).contains(&"session_disposed".to_string()));

        // A goal on a fresh session may be unavailable (no db), but the
        // routing must never fall through to a plain message: `/goal clear`
        // is a handled no-op even with no goal store.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"create_session","sessionId":"s2"}"#,
        );
        cx.run_until_parked();
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s2","text":"/goal clear"}"#,
        );
        cx.run_until_parked();
        let messages = cx.update(|app| state.sessions["s2"].thread.read(app).messages().to_vec());
        assert!(
            messages.is_empty(),
            "a handled slash turn must not insert the raw invocation"
        );

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn exit_while_turn_running_cancels_and_disposes() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        // Seed a session file so the store scan can surface the archived
        // summary; a fresh thread's jsonl materializes only on the first
        // assistant message.
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/");

        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink_out = out.clone();
        let sink = EventSink::new(move |json| sink_out.lock().unwrap().push(json));

        // Make the store aware of s1, then simulate the in-flight turn the
        // event subscription would have flagged.
        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_threads"}"#);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let known = cx.update(|app| {
                let store = agent::thread_store::global();
                let s = store.read(app);
                s.summaries()
                    .iter()
                    .chain(s.archived_summaries())
                    .any(|sum| sum.id == "s1")
            });
            if known {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "seeded session never landed in the store scan"
            );
            thread::sleep(Duration::from_millis(10));
        }
        // Simulate the running turn the event subscription would have
        // flagged: the actor's `turn_active` plus the store's running set.
        state.sessions["s1"]
            .turn_active
            .store(true, Ordering::SeqCst);
        cx.update(|app| {
            let store = agent::thread_store::global();
            store.update(app, |s, cx| s.mark_running("s1", cx));
        });

        // `/exit` while running must cancel the turn and dispose the session
        // immediately instead of silently dropping the command.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"/exit"}"#,
        );
        cx.run_until_parked();
        assert!(!state.sessions.contains_key("s1"));
        assert!(types(&out).contains(&"session_disposed".to_string()));

        // The thread-store summary lands in the archived partition.
        let archived = cx.update(|app| {
            let store = agent::thread_store::global();
            let s = store.read(app);
            s.archived_summaries()
                .iter()
                .any(|sum| sum.id == "s1" && sum.archived)
        });
        assert!(archived, "s1 must be archived in the thread store");

        // The store's running flag is cleared: the disposal drops the
        // session's subscription, so the backend's eventual `TurnFinished`
        // can never reach `mark_idle` — the exit path must do it.
        let running = cx.update(|app| {
            let store = agent::thread_store::global();
            store.read(app).is_running("s1")
        });
        assert!(!running, "s1 must not stay flagged running after /exit");

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// Detaching a running session must not cancel the thread or clear the
    /// store's running flag: a browser refresh would otherwise kill a turn
    /// the desktop surface may still be showing. Contrast with
    /// `dispose_session`, which cancels the turn and resets the store before
    /// releasing it.
    #[test]
    fn detach_running_session_preserves_the_turn() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        // Simulate the running turn the event subscription would have
        // flagged: the actor's `turn_active` plus the store's running set.
        state.sessions["s1"]
            .turn_active
            .store(true, Ordering::SeqCst);
        cx.update(|app| {
            let store = agent::thread_store::global();
            store.update(app, |s, cx| s.mark_running("s1", cx));
        });

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"detach_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();

        assert!(!state.sessions.contains_key("s1"), "session released");
        assert!(types(&out).contains(&"session_disposed".to_string()));
        assert_eq!(
            state.focused.lock().unwrap().as_deref(),
            None,
            "focus cleared"
        );
        // The turn is still streaming for the desktop surface: detach leaves
        // the store's running flag untouched — only `dispose_session` (or the
        // backend's own `TurnFinished`) resets it.
        let running = cx.update(|app| {
            let store = agent::thread_store::global();
            store.read(app).is_running("s1")
        });
        assert!(running, "detach must not clear the store's running flag");

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn running_submit_parks_into_pending_submits() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        state.sessions["s1"]
            .turn_active
            .store(true, Ordering::SeqCst);
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"queued one","clientId":"c1"}"#,
        );
        cx.run_until_parked();

        assert!(
            !types(&out).contains(&"turn_started".to_string()),
            "a parked submit must not start a turn"
        );
        let messages = cx.update(|app| state.sessions["s1"].thread.read(app).messages().to_vec());
        assert!(
            messages.is_empty(),
            "a parked submit must not insert a message"
        );
        assert_eq!(
            state.sessions["s1"].pending_submits.lock().unwrap().len(),
            1
        );

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn settled_turn_drains_parked_submits_into_a_follow_up_run() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        let (engine, events) = FakeEngine::new();
        cx.update(|app| {
            state.sessions["s1"].thread.update(app, |t, cx| {
                t.set_engine_for_test(engine.clone(), events, cx);
            });
        });
        state.sessions["s1"]
            .turn_active
            .store(true, Ordering::SeqCst);
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"follow up","clientId":"c1"}"#,
        );
        cx.run_until_parked();
        assert_eq!(
            state.sessions["s1"].pending_submits.lock().unwrap().len(),
            1
        );

        engine
            .notices
            .send(agent::thread_engine::BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        assert!(
            pump_until(&out, &mut cx, "turn_started", 1),
            "the drained queue starts the follow-up turn"
        );

        assert_eq!(engine.runs.lock().unwrap().as_slice(), ["follow up"]);
        assert!(
            state.sessions["s1"]
                .pending_submits
                .lock()
                .unwrap()
                .is_empty()
        );
        let messages = cx.update(|app| state.sessions["s1"].thread.read(app).messages().to_vec());
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]
                .content
                .iter()
                .any(|c| matches!(c, MessageContent::Text(t) if t == "follow up"))
        );

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn follow_up_turn_emits_turn_finished_before_turn_started() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        let (engine, events) = FakeEngine::new();
        cx.update(|app| {
            state.sessions["s1"].thread.update(app, |t, cx| {
                t.set_engine_for_test(engine.clone(), events, cx);
            });
        });
        state.sessions["s1"]
            .turn_active
            .store(true, Ordering::SeqCst);
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"follow up","clientId":"c1"}"#,
        );
        cx.run_until_parked();

        engine
            .notices
            .send(agent::thread_engine::BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        assert!(pump_until(&out, &mut cx, "turn_started", 1));

        // The drained follow-up must surface as a turn whose start lands
        // after the previous turn's wire finish, so the webview never
        // renders the running follow-up as idle.
        let order: Vec<String> = types(&out)
            .into_iter()
            .filter(|t| t == "turn_finished" || t == "turn_started")
            .collect();
        assert_eq!(order, ["turn_finished", "turn_started"]);
        assert!(
            state.sessions["s1"].turn_active.load(Ordering::SeqCst),
            "the follow-up turn keeps the session turn-active"
        );

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn steer_while_running_emits_steer_pending_and_removes_the_parked_copy() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        let (engine, events) = FakeEngine::new();
        cx.update(|app| {
            state.sessions["s1"].thread.update(app, |t, cx| {
                t.set_engine_for_test(engine.clone(), events, cx);
            });
        });
        // A plain submit starts a real run (the FakeEngine never settles),
        // which flips both the thread's running flag and the actor's
        // turn-active gate.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"first"}"#,
        );
        cx.run_until_parked();
        assert!(types(&out).contains(&"turn_started".to_string()));

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"queued","clientId":"c1"}"#,
        );
        cx.run_until_parked();
        assert_eq!(
            state.sessions["s1"].pending_submits.lock().unwrap().len(),
            1
        );

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"steer","sessionId":"s1","clientId":"c1","text":"steer me"}"#,
        );
        cx.run_until_parked();

        assert_eq!(engine.steer_calls.lock().unwrap().as_slice(), ["steer me"]);
        assert!(
            state.sessions["s1"]
                .pending_submits
                .lock()
                .unwrap()
                .is_empty(),
            "the steered submit must not survive for the turn-end drain"
        );
        let pending = out
            .lock()
            .unwrap()
            .iter()
            .find_map(|raw| {
                let v: Value = serde_json::from_str(raw).unwrap();
                (v["type"] == "steer_pending").then_some(v)
            })
            .expect("steer_pending emitted");
        assert_eq!(pending["clientId"], "c1");
        assert!(
            pending["messageId"].as_str().is_some_and(|s| !s.is_empty()),
            "steer_pending carries the kernel message id"
        );

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn drop_queued_removes_only_the_matching_parked_submit() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        state.sessions["s1"]
            .turn_active
            .store(true, Ordering::SeqCst);
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"keep","clientId":"c1"}"#,
        );
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"submit","sessionId":"s1","text":"drop","clientId":"c2"}"#,
        );
        cx.run_until_parked();
        assert_eq!(
            state.sessions["s1"].pending_submits.lock().unwrap().len(),
            2
        );

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"drop_queued","sessionId":"s1","clientId":"c2"}"#,
        );
        cx.run_until_parked();
        let remaining = state.sessions["s1"].pending_submits.lock().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].client_id, "c1");
        drop(remaining);

        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn idle_steer_falls_back_to_a_regular_message() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"steer","sessionId":"s1","clientId":"c9","text":"steer idle"}"#,
        );
        cx.run_until_parked();

        assert!(types(&out).contains(&"turn_started".to_string()));
        let messages = cx.update(|app| state.sessions["s1"].thread.read(app).messages().to_vec());
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]
                .content
                .iter()
                .any(|c| matches!(c, MessageContent::Text(t) if t == "steer idle"))
        );

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
    fn list_models_replies_after_registration() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        init_globals(&mut cx);
        let (out, sink) = collect_sink();
        let mut state = state_with(PathBuf::from("/"));

        // The command must not answer before the one-shot provider
        // registration finishes (hermetic home: no providers, but the
        // snapshot still lands so waiting surfaces leave their disabled
        // state).
        cmd(&mut cx, &mut state, &sink, r#"{"cmd":"list_models"}"#);
        assert!(pump_until(&out, &mut cx, "models", 1));
        let event: Value = serde_json::from_str(
            out.lock()
                .unwrap()
                .iter()
                .find(|raw| raw.contains("\"type\":\"models\""))
                .expect("models event present"),
        )
        .expect("models event is valid json");
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
        assert_eq!(value["provider_name"], "anthropic");
        assert_eq!(value["api"], "anthropic");
        assert_eq!(value["context_window"], 200_000);
        assert!(value["name"].is_string());
    }

    #[test]
    fn deduped_models_keeps_wire_variants() {
        let model = |provider: &str, display: Option<&str>, api: &str, id: &str| {
            let mut metadata = HashMap::new();
            if let Some(display) = display {
                metadata.insert("provider_display_name".to_string(), json!(display));
            }
            pi::types::Model {
                provider: provider.into(),
                api: api.into(),
                id: id.into(),
                context_window: 200_000,
                max_tokens: 8192,
                thinking: pi::types::ThinkingKind::None,
                metadata,
            }
        };
        let deduped = deduped_models(vec![
            // Wire variants of one provider: same display name + id, distinct
            // registration names ({name}-{wire}) — all kept so the picker can
            // offer the responses/completions endpoints.
            model(
                "DeepSeek-anthropic",
                Some("DeepSeek"),
                "anthropic",
                "deepseek-v4-pro",
            ),
            model(
                "DeepSeek-responses",
                Some("DeepSeek"),
                "openai_responses",
                "deepseek-v4-pro",
            ),
            // An exact registration duplicate collapses to the first.
            model(
                "DeepSeek-anthropic",
                Some("DeepSeek"),
                "anthropic",
                "deepseek-v4-pro",
            ),
            model(
                "DeepSeek-completions",
                Some("DeepSeek"),
                "openai_completions",
                "deepseek-v4-pro",
            ),
            // Same registration name, distinct id: kept.
            model(
                "DeepSeek-anthropic",
                Some("DeepSeek"),
                "anthropic",
                "deepseek-v4-flash",
            ),
            // No display metadata: identity is the registration name.
            model("openai", None, "anthropic", "gpt-5"),
        ]);
        let identities: Vec<(String, String, String)> = deduped
            .iter()
            .map(|m| {
                (
                    agent::pi_providers::display_provider_name(m),
                    m.api.clone(),
                    m.id.clone(),
                )
            })
            .collect();
        assert_eq!(
            identities,
            vec![
                (
                    "DeepSeek".to_string(),
                    "anthropic".to_string(),
                    "deepseek-v4-pro".to_string()
                ),
                (
                    "DeepSeek".to_string(),
                    "openai_responses".to_string(),
                    "deepseek-v4-pro".to_string()
                ),
                (
                    "DeepSeek".to_string(),
                    "openai_completions".to_string(),
                    "deepseek-v4-pro".to_string()
                ),
                (
                    "DeepSeek".to_string(),
                    "anthropic".to_string(),
                    "deepseek-v4-flash".to_string()
                ),
                (
                    "openai".to_string(),
                    "anthropic".to_string(),
                    "gpt-5".to_string()
                ),
            ]
        );
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

    #[test]
    fn plan_and_goal_commands_route_on_a_session() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);

        // set_plan_mode flips the thread's plan-mode flag.
        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"set_plan_mode","sessionId":"s1","enabled":true}"#,
        );
        cx.run_until_parked();
        assert!(cx.update(|app| state.sessions["s1"].thread.read(app).plan_mode()));

        // plan_verdict without a pending review surfaces a clear error.
        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"plan_verdict","sessionId":"s1","choice":"execute_keep"}"#,
        );
        cx.run_until_parked();
        assert!(out.lock().unwrap().iter().any(|raw| {
            serde_json::from_str::<serde_json::Value>(raw).unwrap()["message"]
                == "no pending plan review"
        }));

        // plan_seed_execution with an unreadable plan file fails explicitly
        // instead of seeding an empty plan context.
        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"plan_seed_execution","sessionId":"s1","planFile":"/nonexistent/manox-plan.md"}"#,
        );
        cx.run_until_parked();
        assert!(types(&out).contains(&"error".to_string()));

        // goal create on a thread without a goal store surfaces an error
        // (the hermetic env has no db); the command never panics.
        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"goal","sessionId":"s1","action":"create","objective":"ship it"}"#,
        );
        cx.run_until_parked();
        assert!(types(&out).contains(&"error".to_string()));

        // Unknown session for the new commands still reports an error.
        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"stop_background_task","sessionId":"nope","taskId":"mon_1"}"#,
        );
        assert!(types(&out).contains(&"error".to_string()));

        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"dispose_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn plan_ready_emits_review_card_and_records_the_pending_verdict() {
        let _guard = GLOBALS_LOCK.lock().unwrap();
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);
        let sink = sink_for(&out);

        // A proposed plan surfaces as the enriched wire event the sidebar's
        // review card renders from.
        let dir = std::env::temp_dir().join(format!("manox-plan-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan_file = dir.join("audit-plan.md");
        std::fs::write(&plan_file, "# Audit\n\nsteps").unwrap();
        let plan_file = plan_file.to_string_lossy().to_string();
        cx.update(|app| {
            state.sessions["s1"].thread.update(app, |_t, cx| {
                cx.emit(agent::ThreadEvent::PlanReady {
                    plan_file: plan_file.clone(),
                    title: "Audit".into(),
                });
            });
        });
        cx.run_until_parked();
        let ready = out
            .lock()
            .unwrap()
            .iter()
            .filter_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .find(|v| v["type"] == "plan_ready")
            .expect("plan_ready wire event");
        assert_eq!(ready["sessionId"], "s1");
        assert_eq!(ready["plan_file"], plan_file.as_str());
        assert_eq!(ready["title"], "Audit");
        assert_eq!(ready["content"], "# Audit\n\nsteps");

        // The recorded pending plan makes a verdict succeed (refine keeps
        // plan mode on and consumes the card).
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"plan_verdict","sessionId":"s1","choice":"refine"}"#,
        );
        cx.run_until_parked();
        assert!(!out.lock().unwrap().iter().any(|raw| {
            serde_json::from_str::<serde_json::Value>(raw).unwrap()["message"]
                == "no pending plan review"
        }));
        // A second verdict now has nothing to consume.
        cmd(
            &mut cx,
            &mut state,
            &sink,
            r#"{"cmd":"plan_verdict","sessionId":"s1","choice":"refine"}"#,
        );
        cx.run_until_parked();
        assert!(out.lock().unwrap().iter().any(|raw| {
            serde_json::from_str::<serde_json::Value>(raw).unwrap()["message"]
                == "no pending plan review"
        }));

        cmd(
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
    fn open_thread_replay_reemits_pending_plan_review() {
        // Recover from a poisoned lock: an earlier flaky test's panic can
        // leave the serialization mutex poisoned; this test must still run.
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);

        // A proposed plan records the pending verdict; the enriched wire
        // event lands once.
        let dir = std::env::temp_dir().join(format!("manox-plan-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan_file = dir.join("audit-plan.md");
        std::fs::write(&plan_file, "# Audit\n\nsteps").unwrap();
        let plan_file = plan_file.to_string_lossy().to_string();
        cx.update(|app| {
            state.sessions["s1"].thread.update(app, |_t, cx| {
                cx.emit(agent::ThreadEvent::PlanReady {
                    plan_file: plan_file.clone(),
                    title: "Audit".into(),
                });
            });
        });
        cx.run_until_parked();
        let ready_count = || {
            out.lock()
                .unwrap()
                .iter()
                .filter_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .filter(|v| v["type"] == "plan_ready" && v["plan_file"] == plan_file)
                .count()
        };
        assert_eq!(ready_count(), 1);

        // Reopening the live session replays its snapshots; a reloaded
        // webview gets the review card re-emitted instead of losing it.
        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"open_thread","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        assert_eq!(
            ready_count(),
            2,
            "open_thread replay must re-emit the pending plan review"
        );

        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"dispose_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// A webview reload re-emits a pending interaction as the ask surface:
    /// a parked entry's `PendingAuthMeta.tool_name` may differ (bubbled
    /// team-member auth), but the replay always surfaces
    /// `AskUserQuestion` — matching the live shape so the ask card renders
    /// identically after a reload.
    #[test]
    fn open_thread_replay_reemits_pending_auth_as_ask_surface() {
        let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        hermetic_home();
        let mut cx = HeadlessAppContext::new(Arc::new(gpui::NoopTextSystem));
        cx.allow_parking();
        let mut state = state_with(PathBuf::from("/"));
        let out = with_session_for_submit(&mut cx, &mut state);

        // Swap in a fake engine carrying a parked interaction whose stored
        // tool_name is not `AskUserQuestion` (bubbled member auth shape).
        // The replay must still surface `AskUserQuestion` for this id.
        let (engine, events) = FakeEngine::new();
        engine.set_pending_auth(
            "ask-1",
            "Bash",
            "Clarify the next step",
            json!({
                "questions": [{
                    "question": "Which approach should I take?",
                    "header": "Clarify",
                    "multiSelect": false,
                    "options": [
                        {"label": "Option A"},
                        {"label": "Option B"}
                    ]
                }]
            }),
        );
        cx.update(|app| {
            state.sessions["s1"].thread.update(app, |t, cx| {
                t.set_engine_for_test(engine.clone(), events, cx);
            });
        });
        cx.run_until_parked();

        let auth_events = |tool_name: &str| {
            out.lock()
                .unwrap()
                .iter()
                .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
                .filter(|v| v["type"] == "tool_call_authorization" && v["id"] == "ask-1")
                .filter(|v| v["tool_name"].as_str() == Some(tool_name))
                .count()
        };
        assert_eq!(
            auth_events("Bash"),
            0,
            "the replay must not surface the original tool name"
        );

        // Reopen the live session: the replayed interaction must arrive as
        // the ask surface (`AskUserQuestion`), matching the live shape.
        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"open_thread","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        assert_eq!(
            auth_events("AskUserQuestion"),
            1,
            "open_thread replay must re-emit the pending auth as the ask surface"
        );

        cmd(
            &mut cx,
            &mut state,
            &sink_for(&out),
            r#"{"cmd":"dispose_session","sessionId":"s1"}"#,
        );
        cx.run_until_parked();
        drop(state);
        agent::thread_store::drop_global_for_test();
    }

    /// A sink that feeds an already-collected `out` buffer.
    fn sink_for(out: &Arc<Mutex<Vec<String>>>) -> EventSink {
        let out = Arc::clone(out);
        EventSink::new(move |json| out.lock().unwrap().push(json))
    }

    /// Scripted backend for the queue/steer tests: records `run`/`steer`
    /// traffic and lets the test drive settlement through the notice
    /// channel, so the actor pipeline is exercised without a live provider.
    struct FakeEngine {
        runs: Mutex<Vec<String>>,
        steer_calls: Mutex<Vec<String>>,
        notices: tokio::sync::mpsc::UnboundedSender<agent::thread_engine::BackendNotice>,
        pending_auth: Mutex<Vec<(String, agent::permission::PendingAuthMeta)>>,
    }

    impl FakeEngine {
        fn new() -> (
            Arc<Self>,
            tokio::sync::mpsc::UnboundedReceiver<agent::thread_engine::BackendNotice>,
        ) {
            let (notices, events) = tokio::sync::mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    runs: Mutex::new(Vec::new()),
                    steer_calls: Mutex::new(Vec::new()),
                    notices,
                    pending_auth: Mutex::new(Vec::new()),
                }),
                events,
            )
        }

        /// Seed a pending interaction the workspace re-surfaces on
        /// switch-back / replay. `tool_name` is a non-`AskUserQuestion`
        /// name here so the replay proves it re-emits as the ask surface
        /// regardless.
        fn set_pending_auth(&self, id: &str, tool_name: &str, summary: &str, input: Value) {
            *self.pending_auth.lock().unwrap() = vec![(
                id.to_string(),
                agent::permission::PendingAuthMeta {
                    tool_name: tool_name.to_string(),
                    summary: summary.to_string(),
                    input,
                },
            )];
        }
    }

    impl agent::thread_engine::ThreadEngine for FakeEngine {
        fn is_running(&self) -> bool {
            false
        }

        fn history(&self) -> Vec<agent::db::HistoryEntry> {
            Vec::new()
        }

        fn request_token_usage(&self) -> HashMap<String, agent::TokenUsage> {
            HashMap::new()
        }

        fn model(&self) -> Option<pi::types::Model> {
            None
        }

        fn run(&self, prompt: String, _images: Vec<pi::types::ContentBlock>) {
            self.runs.lock().unwrap().push(prompt);
        }

        fn steer(&self, text: String, _images: Vec<pi::types::ContentBlock>) -> String {
            self.steer_calls.lock().unwrap().push(text);
            String::new()
        }

        fn cancel_steer(&self, _id: &str) -> bool {
            false
        }

        fn abort(&self) {}

        fn set_model(&self, _model: pi::types::Model) {}

        fn set_thinking_level(&self, _level: Option<String>) {}

        fn open_session(&self, _path: PathBuf) {}

        fn new_session(&self, _cwd: PathBuf, _project: Option<PathBuf>) {}

        fn active_session_path(&self) -> Option<PathBuf> {
            None
        }

        fn session_list(&self) -> Vec<agent::ThreadSummary> {
            Vec::new()
        }

        fn pending_auth_entries(&self) -> Vec<(String, agent::permission::PendingAuthMeta)> {
            self.pending_auth.lock().unwrap().clone()
        }
    }
}
