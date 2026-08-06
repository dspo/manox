//! The pi harness engine: drives a `pi::coding_agent::AgentSession` from a
//! tokio actor and adapts its events onto the UI's `ThreadEvent` language.
//!
//! This is the first-class harness backend behind the `Thread` facade: the
//! facade holds an `Arc<dyn ThreadEngine>` (this `PiEngine`), spawns the
//! actor, and drains `BackendNotice`s on the gpui thread. Pure mappings
//! between pi wire types and the UI language live here (adapt), so the
//! facade only ever sees `Message` / `ThreadEvent`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi::coding_agent::{AgentSession, ModelRuntime, create_agent_session};
use pi::ext_point_agent::AgentRegistry;
use pi::tool::AgentTool as PiAgentTool;
use pi::types::{AgentEvent, AgentMessage, ContentBlock, Model as PiModel};
use pi_extensions::agents::{SubagentTool, register_defaults};
use pi_extensions::bash::BashTool;
use pi_extensions::bash::orchestration::BackgroundManager;
use pi_extensions::bash::persistent::PersistentShellOperations;
use pi_extensions::{BackgroundRegistry, BashOutputTool, TaskStopTool};
use tokio::sync::mpsc;

use crate::db::ThreadSummary;
use crate::language_model::{MessageContent, TokenUsage};
use crate::message::Message;
use crate::thread::ThreadEvent;
use crate::thread_engine::{BackendNotice, SpawnedEngine, ThreadEngine};

/// Commands the gpui side sends to the pi actor.
enum SessionCmd {
    /// Start a turn with the given user text.
    Prompt(String),
    /// Inject a steer into the running turn.
    Steer { id: String, text: String },
    /// Retract a queued steer.
    CancelSteer(String),
    /// Abort the running turn.
    Abort,
    /// Hot-swap the model for the next provider request.
    SetModel(PiModel),
    /// Map the reasoning effort onto pi's thinking level.
    SetThinkingLevel(Option<String>),
    /// Re-point the session at an existing jsonl file.
    Open { path: PathBuf },
    /// Create a fresh session in the given directory.
    NewSession { cwd: PathBuf },
    /// Close the session and stop the actor.
    Shutdown,
}

// BackendNotice is the shared facade/backend contract (thread_engine.rs);
// the actor sends it over the notice channel the facade drains.

/// Authoritative state the actor writes and the facade mirrors.
struct EngineState {
    running: AtomicBool,
    history: Mutex<Vec<Message>>,
    request_usage: Mutex<HashMap<String, TokenUsage>>,
    cumulative: Mutex<TokenUsage>,
    per_model: Mutex<HashMap<String, TokenUsage>>,
    model: Mutex<Option<PiModel>>,
    sessions: Mutex<Vec<ThreadSummary>>,
    active_path: Mutex<Option<PathBuf>>,
}

/// The pi harness backend behind the `Thread` facade.
pub struct PiEngine {
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    state: Arc<EngineState>,
}

/// Spawn the pi actor and return the engine handle plus its notice receiver.
/// The facade drains the receiver on the gpui thread. `initial_path`, when
/// given, opens that session file instead of restoring the newest one.
pub fn spawn_engine(
    cwd: PathBuf,
    model: Option<PiModel>,
    sessions_dir: PathBuf,
    initial_path: Option<PathBuf>,
    fresh: bool,
) -> SpawnedEngine {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (notice_tx, notice_rx) = mpsc::unbounded_channel();
    let state = Arc::new(EngineState {
        running: AtomicBool::new(false),
        history: Mutex::new(Vec::new()),
        request_usage: Mutex::new(HashMap::new()),
        cumulative: Mutex::new(TokenUsage::default()),
        per_model: Mutex::new(HashMap::new()),
        model: Mutex::new(model.clone()),
        sessions: Mutex::new(Vec::new()),
        active_path: Mutex::new(initial_path.clone()),
    });
    crate::runtime::handle().spawn(run_actor(
        cwd,
        model,
        sessions_dir,
        initial_path,
        fresh,
        cmd_rx,
        notice_tx,
        Arc::clone(&state),
    ));
    SpawnedEngine {
        engine: Arc::new(PiEngine { cmd_tx, state }),
        events: notice_rx,
    }
}

impl ThreadEngine for PiEngine {
    fn is_running(&self) -> bool {
        self.state.running.load(Ordering::Relaxed)
    }

    fn history(&self) -> Vec<Message> {
        self.state.history.lock().unwrap().clone()
    }

    fn request_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.state.request_usage.lock().unwrap().clone()
    }

    fn cumulative_token_usage(&self) -> TokenUsage {
        *self.state.cumulative.lock().unwrap()
    }

    fn per_model_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.state.per_model.lock().unwrap().clone()
    }

    fn model(&self) -> Option<PiModel> {
        self.state.model.lock().unwrap().clone()
    }

    fn run(&self, prompt: String) {
        let _ = self.cmd_tx.send(SessionCmd::Prompt(prompt));
    }

    fn steer(&self, text: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let _ = self.cmd_tx.send(SessionCmd::Steer {
            id: id.clone(),
            text,
        });
        id
    }

    fn cancel_steer(&self, id: &str) -> bool {
        // Optimistic: the actor retracts the steer asynchronously. True means
        // the retraction was queued, not that the message is gone from the
        // transcript — it may already have been drained into the running turn.
        let _ = self.cmd_tx.send(SessionCmd::CancelSteer(id.to_string()));
        true
    }

    fn abort(&self) {
        let _ = self.cmd_tx.send(SessionCmd::Abort);
    }

    fn set_model(&self, model: PiModel) {
        let _ = self.cmd_tx.send(SessionCmd::SetModel(model));
    }

    fn set_thinking_level(&self, level: Option<String>) {
        let _ = self.cmd_tx.send(SessionCmd::SetThinkingLevel(level));
    }

    fn open_session(&self, path: PathBuf) {
        let _ = self.cmd_tx.send(SessionCmd::Open { path });
    }

    fn new_session(&self, cwd: PathBuf) {
        let _ = self.cmd_tx.send(SessionCmd::NewSession { cwd });
    }

    fn active_session_path(&self) -> Option<PathBuf> {
        self.state.active_path.lock().unwrap().clone()
    }

    fn session_list(&self) -> Vec<ThreadSummary> {
        self.state.sessions.lock().unwrap().clone()
    }

    fn shutdown(&self) {
        let _ = self.cmd_tx.send(SessionCmd::Shutdown);
    }
}

// ── The pi actor ───────────────────────────────────────────────────────────

/// Minimal builtin coding-agent prompt. Deliberately not the manox
/// `system_prompt` assembly — that belongs to the manox harness.
fn system_prompt(cwd: &Path) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d");
    format!(
        "You are Manox Pi, a coding agent running inside the manox app on the pi harness.\n\
         Working directory: {cwd}\n\
         Date: {date}\n\n\
         Use your tools to inspect, edit, and create files and to run shell commands.\n\
         Make changes directly, keep replies concise, and verify your work when practical.",
        cwd = cwd.display(),
    )
}

/// The full pi toolset: pi's file tools plus the pi-extensions bash/sub-agent
/// orchestration (assembly mirrors the `pi-extensions` orchestration example).
fn build_tools(
    cwd: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
) -> Vec<Arc<dyn PiAgentTool>> {
    let background = Arc::new(BackgroundRegistry::new());
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let bash = BashTool::new(
        Arc::new(PersistentShellOperations::new(cwd)),
        background.clone(),
    )
    .with_manager(Arc::clone(&manager));

    let mut tools: Vec<Arc<dyn PiAgentTool>> = vec![
        Arc::new(pi::tools::read::ReadTool),
        Arc::new(pi::tools::write::WriteTool),
        Arc::new(pi::tools::edit::EditTool),
        Arc::new(pi::tools::grep::GrepTool),
        Arc::new(pi::tools::find::FindTool),
        Arc::new(pi::tools::ls::LsTool),
        Arc::new(bash),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(TaskStopTool::new(background)),
    ];
    // The sub-agent tool needs a concrete model; a session assembled before
    // registration landed (first seconds after launch) skips it.
    if let Some(model) = model {
        let mut registry = AgentRegistry::new();
        register_defaults(&mut registry);
        let subagent = SubagentTool::new(
            Arc::new(registry),
            vec![
                Arc::new(pi::tools::read::ReadTool),
                Arc::new(pi::tools::grep::GrepTool),
                Arc::new(pi::tools::find::FindTool),
                Arc::new(pi::tools::ls::LsTool),
            ],
        )
        .with_model_runtime(runtime.clone())
        .with_model(model.clone());
        tools.push(Arc::new(subagent));
    }
    tools
}

fn steer_message(text: String) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text,
            signature: None,
        }],
        timestamp: chrono::Utc::now(),
    }
}

/// Forward every pi run event through the adapt mapping onto the notice
/// channel as UI events.
fn subscribe_session(
    session: &AgentSession,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) -> pi::agent::Subscription {
    let event_tx = notice_tx.clone();
    session.subscribe(Arc::new(move |event, _cancel| {
        let tx = event_tx.clone();
        Box::pin(async move {
            for te in adapt::agent_event_to_thread_events(&event) {
                let _ = tx.send(BackendNotice::Event(Box::new(te)));
            }
        })
    }))
}

/// Build the session builder against the given project dir, using the shared
/// runtime and model.
fn session_builder(
    cwd: &Path,
    sessions_dir: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
) -> pi::coding_agent::AgentSessionBuilder {
    let mut builder = create_agent_session()
        .with_cwd(cwd.to_path_buf())
        .with_session_dir(sessions_dir.to_path_buf())
        .with_model_runtime(runtime.clone())
        .with_system_prompt(system_prompt(cwd))
        .with_tools(build_tools(cwd, runtime, model));
    if let Some(model) = model {
        builder = builder.with_model(model.clone());
    }
    builder
}

#[allow(clippy::too_many_arguments)] // actor entry: startup options stay explicit
async fn run_actor(
    cwd: PathBuf,
    model: Option<PiModel>,
    sessions_dir: PathBuf,
    initial_path: Option<PathBuf>,
    fresh: bool,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    state: Arc<EngineState>,
) {
    let registry = crate::pi_providers::global();
    let runtime = ModelRuntime::with_provider_registry(registry.clone()).with_catalog(Arc::new(
        crate::pi_providers::LegacyAliasCatalog::new(registry.clone()),
    ));
    let mut pi_model = model.or_else(crate::pi_providers::default_model);
    // Session assembly preflights the model against the registry, so an
    // unresolvable model cannot build. Right after launch the background
    // registration (parallelized per provider) may still be in flight;
    // wait for it only when no model is resolvable yet.
    if pi_model.is_none() {
        crate::pi_providers::wait_ready().await;
        pi_model = crate::pi_providers::default_model();
    }
    let Some(pi_model) = pi_model else {
        let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
            "no model configured — add a provider in Settings"
        )));
        return;
    };

    // Restore the requested session, else the newest one, else start fresh.
    // Tool cwd follows the restored session's project dir (the builder's
    // `open` re-pins cwd too).
    let repo = pi::session::repository::SessionRepository::new(&sessions_dir);
    // `fresh` threads (sidebar new-conversation, project-bound creation)
    // never inherit the previous session; startup and explicit opens do.
    let latest = if fresh {
        None
    } else {
        repo.list().await.ok().and_then(|list| {
            if let Some(requested) = &initial_path {
                return list.into_iter().find(|info| info.path == *requested);
            }
            list.into_iter().find(|info| info.message_count > 0)
        })
    };
    let mut restored = false;
    let mut session = None;
    if let Some(info) = latest {
        // Sessions created by a GUI launch (process cwd `/`) persisted a
        // useless cwd; heal them to this launch's default instead.
        let mut tool_cwd = PathBuf::from(info.cwd.clone());
        if tool_cwd.as_os_str() == "/" {
            tool_cwd = cwd.clone();
        }
        let builder = session_builder(&tool_cwd, &sessions_dir, &runtime, Some(&pi_model));
        match builder.open(info.path).await {
            Ok(s) => {
                restored = true;
                session = Some(s);
            }
            Err(err) => {
                tracing::warn!("pi session restore failed ({err}); starting fresh");
            }
        }
    }
    let mut session = match session {
        Some(s) => s,
        None => {
            let builder = session_builder(&cwd, &sessions_dir, &runtime, Some(&pi_model));
            match builder.build().await {
                Ok(s) => s,
                Err(err) => {
                    let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                        "pi session build failed: {err}"
                    )));
                    return;
                }
            }
        }
    };
    *state.active_path.lock().unwrap() = Some(session.path().to_path_buf());
    refresh_session_list(&repo, &state).await;

    // Stream run events back to the gpui drainer. Re-registered after a
    // session rebuild (listeners live on the old Agent).
    let mut _subscription = subscribe_session(&session, &notice_tx);

    let _ = notice_tx.send(BackendNotice::Ready {
        restored,
        model: Some(pi_model.clone()),
    });
    if restored {
        sync_history(&session, &state);
        sync_usage(&session, &state).await;
    }

    let mut run_steers: Vec<String> = Vec::new();
    let mut shutdown_after_run = false;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCmd::Prompt(text) => {
                state.running.store(true, Ordering::Relaxed);
                let handle = session.handle();
                let mut abort_requested = false;
                let mut channel_open = true;
                // Drive the run while still servicing mid-run commands
                // (abort/steer) through the session handle.
                let result = {
                    let prompt = session.prompt(&text);
                    tokio::pin!(prompt);
                    loop {
                        if !channel_open {
                            break prompt.await;
                        }
                        tokio::select! {
                            maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                                Some(SessionCmd::Abort) => {
                                    abort_requested = true;
                                    handle.abort();
                                }
                                Some(SessionCmd::Steer { id, text }) => {
                                    handle.steer(steer_message(text));
                                    run_steers.push(id);
                                }
                                Some(SessionCmd::CancelSteer(id)) => {
                                    handle.cancel_steer(&id);
                                }
                                Some(SessionCmd::Shutdown) => shutdown_after_run = true,
                                Some(_) => {} // queued prompts/reconfigs wait for settle
                                None => {
                                    // Facade dropped mid-run: abort, settle, exit.
                                    channel_open = false;
                                    shutdown_after_run = true;
                                    if !abort_requested {
                                        abort_requested = true;
                                        handle.abort();
                                    }
                                }
                            },
                            result = &mut prompt => break result,
                        }
                    }
                };

                let failed = result.is_err();
                if let Err(err) = &result {
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                        anyhow::anyhow!("{err:#}"),
                    ))));
                }
                state.running.store(false, Ordering::Relaxed);
                sync_history(&session, &state);
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
                let (steered, stranded) = if abort_requested || failed {
                    (Vec::new(), std::mem::take(&mut run_steers))
                } else {
                    (std::mem::take(&mut run_steers), Vec::new())
                };
                let _ = notice_tx.send(BackendNotice::Settled {
                    cancelled: abort_requested,
                    failed,
                    steered,
                    stranded,
                });
                if shutdown_after_run {
                    break;
                }
            }
            SessionCmd::Steer { id, text } => {
                // A steer queued while idle is injected into the next turn;
                // confirmation (SteerInjected) rides that turn's settlement.
                session.handle().steer(steer_message(text));
                run_steers.push(id);
            }
            SessionCmd::CancelSteer(id) => {
                session.handle().cancel_steer(&id);
            }
            SessionCmd::Abort => {
                session.abort();
            }
            SessionCmd::SetModel(pi_model) => {
                // Streams dispatch by `model.provider` through the shared
                // registry, so a cross-provider switch reaches the right
                // endpoint + credential (the old bridge captured the
                // initial model's credential for every later model).
                if let Err(err) = session.set_model(pi_model.clone()).await {
                    tracing::warn!("pi set_model failed: {err}");
                }
                *state.model.lock().unwrap() = Some(pi_model);
            }
            SessionCmd::SetThinkingLevel(level) => {
                if let Err(err) = session.set_thinking_level(level).await {
                    tracing::warn!("pi set_thinking_level failed: {err}");
                }
            }
            SessionCmd::Open { path } => {
                rebuild_session(
                    &mut session,
                    &path,
                    &sessions_dir,
                    &runtime,
                    &pi_model,
                    &cwd,
                    &notice_tx,
                )
                .await;
                _subscription = subscribe_session(&session, &notice_tx);
                *state.active_path.lock().unwrap() = Some(path);
                sync_history(&session, &state);
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
            }
            SessionCmd::NewSession { cwd } => {
                let builder = session_builder(&cwd, &sessions_dir, &runtime, Some(&pi_model));
                match builder.build().await {
                    Ok(s) => {
                        session = s;
                        _subscription = subscribe_session(&session, &notice_tx);
                        *state.active_path.lock().unwrap() = Some(session.path().to_path_buf());
                        sync_history(&session, &state);
                        sync_usage(&session, &state).await;
                        refresh_session_list(&repo, &state).await;
                    }
                    Err(err) => {
                        let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                            "pi session create failed: {err}"
                        )));
                        return;
                    }
                }
            }
            SessionCmd::Shutdown => break,
        }
    }

    let _ = session.close().await;
}

/// Close the current session and open the given jsonl file in its place. The
/// project dir comes from the session's own record so tools re-pin to the
/// project the session was started in.
async fn rebuild_session(
    session: &mut AgentSession,
    path: &Path,
    sessions_dir: &Path,
    runtime: &ModelRuntime,
    model: &PiModel,
    fallback_cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    // The old session is replaced (its Drop runs on the actor thread); it is
    // already idle when a switch happens, so nothing in-flight is lost.
    let repo = pi::session::repository::SessionRepository::new(sessions_dir);
    let cwd = repo
        .list()
        .await
        .ok()
        .and_then(|list| {
            list.into_iter()
                .find(|info| info.path == path)
                .map(|info| PathBuf::from(info.cwd))
        })
        .map(|cwd| {
            if cwd.as_os_str() == "/" {
                fallback_cwd.to_path_buf()
            } else {
                cwd
            }
        })
        .unwrap_or_else(|| fallback_cwd.to_path_buf());
    let builder = session_builder(&cwd, sessions_dir, runtime, Some(model));
    match builder.open(path.to_path_buf()).await {
        Ok(s) => *session = s,
        Err(err) => {
            let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                "pi session open failed: {err}"
            )));
        }
    }
}

/// Mirror the session's authoritative transcript into engine state.
fn sync_history(session: &AgentSession, state: &Arc<EngineState>) {
    let mapped = adapt::harness_messages_to_messages(session.harness_messages());
    *state.history.lock().unwrap() = mapped;
}

/// Mirror session usage into the engine state. Cumulative and per-model
/// totals come from the kernel's stats over the full active branch —
/// compacted history, tool results, and summarization usage included (TS
/// `getSessionStats` semantics: totals reflect what was actually billed).
/// Only the per-request attribution for the env card stays a thin
/// presentation walk here.
async fn sync_usage(session: &AgentSession, state: &Arc<EngineState>) {
    let stats = match session.session_stats().await {
        Ok(stats) => stats,
        Err(err) => {
            // Degrade to the assistant-only walk (the pre-stats mechanism)
            // so a failing stats read never freezes UI usage at stale
            // values. Loses tool-result/summary usage for this sync only.
            tracing::warn!("pi session stats failed; falling back to message walk: {err:#}");
            sync_usage_from_messages(session, state);
            return;
        }
    };
    let cumulative = token_usage_from_totals(&stats.tokens);
    let mut per_model = HashMap::new();
    for entry in &stats.per_model {
        per_model.insert(entry.key.clone(), token_usage_from_totals(&entry.totals));
    }
    *state.cumulative.lock().unwrap() = cumulative;
    *state.per_model.lock().unwrap() = per_model;
    *state.request_usage.lock().unwrap() = request_attribution(session);
}

/// Per-request attribution for the env card: the assistant usage of the
/// transcript, attributed to the triggering (most recent) user message.
/// Presentation-layer accounting, hence host-side.
fn request_attribution(session: &AgentSession) -> HashMap<String, TokenUsage> {
    let mut request: HashMap<String, TokenUsage> = HashMap::new();
    let key = last_user_id(session);
    for m in session.harness_messages() {
        let AgentMessage::Assistant { usage, .. } = m else {
            continue;
        };
        let u = to_token_usage(usage);
        if u.total_tokens() == 0 {
            continue;
        }
        request
            .entry(key.clone())
            .and_modify(|acc| *acc = *acc + u)
            .or_insert(u);
    }
    request
}

/// Fallback aggregation when `session_stats()` is unavailable: assistant
/// usage only, keyed by model id (the pre-stats mechanism).
fn sync_usage_from_messages(session: &AgentSession, state: &Arc<EngineState>) {
    let mut cumulative = TokenUsage::default();
    let mut per_model: HashMap<String, TokenUsage> = HashMap::new();
    for m in session.harness_messages() {
        let AgentMessage::Assistant { usage, model, .. } = m else {
            continue;
        };
        let u = to_token_usage(usage);
        if u.total_tokens() == 0 {
            continue;
        }
        cumulative = cumulative + u;
        per_model
            .entry(model.clone())
            .and_modify(|acc| *acc = *acc + u)
            .or_insert(u);
    }
    *state.cumulative.lock().unwrap() = cumulative;
    *state.per_model.lock().unwrap() = per_model;
    *state.request_usage.lock().unwrap() = request_attribution(session);
}

/// Kernel usage totals → the facade's token usage shape.
fn token_usage_from_totals(t: &pi::coding_agent::usage::UsageTotals) -> TokenUsage {
    TokenUsage {
        input_tokens: t.input,
        output_tokens: t.output,
        cache_creation_input_tokens: t.cache_write,
        cache_read_input_tokens: t.cache_read,
    }
}

/// The message id of the most recent user message, as the facade's history
/// assigns it. Usage is attributed to the turn's triggering user message.
fn last_user_id(session: &AgentSession) -> String {
    let mapped = adapt::harness_messages_to_messages(session.harness_messages());
    mapped
        .iter()
        .rev()
        .find(|m| matches!(m.role, crate::language_model::Role::User))
        .map(|m| m.id.clone())
        .unwrap_or_default()
}

/// Map a pi usage report onto the manox token shape.
fn to_token_usage(u: &pi::types::Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
    }
}

/// Re-read the session directory and mirror the summary list into engine
/// state (the sidebar's source of truth).
async fn refresh_session_list(
    repo: &pi::session::repository::SessionRepository,
    state: &Arc<EngineState>,
) {
    let mut out = Vec::new();
    if let Ok(list) = repo.list().await {
        for info in list {
            out.push(session_info_to_summary(&info));
        }
    }
    *state.sessions.lock().unwrap() = out;
}

/// Map a pi session info onto the sidebar summary shape.
fn session_info_to_summary(info: &pi::session::repository::SessionInfo) -> ThreadSummary {
    ThreadSummary {
        id: info.id.clone(),
        summary: info.first_message.clone(),
        title: None,
        title_override: None,
        model_id: String::new(),
        provider_id: None,
        approval_mode: 0,
        project: if info.cwd == "/" {
            String::new()
        } else {
            info.cwd.clone()
        },
        depth: 0,
        parent_id: info.parent_session_path.clone(),
        archived: false,
        pinned: false,
        has_unread: false,
        errored: false,
        created_at: info.created_at.timestamp(),
        interacted_at: info.modified_at.timestamp(),
        updated_at: info.modified_at.timestamp(),
        cumulative_total_tokens: 0,
    }
}

// ── Pure mappings between pi wire types and the UI language ────────────────

/// Pure mappings between pi harness wire types and the UI's language.
///
/// The facade renders two data shapes: the `ThreadEvent` stream (live deltas)
/// and `agent::Message` history (rebuild). A pi `AgentSession` produces
/// `AgentEvent`s and `AgentMessage`s; the functions here translate them into
/// those two shapes so the polished manox render pipeline is reused.
pub(crate) mod adapt {
    use super::*;
    use crate::language_model::{
        LanguageModelToolResult, LanguageModelToolUse, StopReason as ManoxStopReason,
    };
    use crate::thread::ToolCallStatus;
    use pi::types::StopReason as PiStopReason;

    /// Map one pi `AgentEvent` onto the `ThreadEvent`s the workspace renders.
    ///
    /// Events with no UI counterpart (run/turn lifecycle handled by the facade,
    /// message boundaries, block start/end markers) map to nothing. The
    /// manox-only variants (`ToolCallAuthorization`, `Plan*`, sub-agent events)
    /// are never produced — approval and plan flows are not wired in this stage.
    pub fn agent_event_to_thread_events(event: &AgentEvent) -> Vec<ThreadEvent> {
        match event {
            AgentEvent::AgentStart | AgentEvent::AgentEnd { .. } => Vec::new(),
            // `TurnStarted` is emitted once by the facade (matching `Thread`
            // semantics); pi's per-round `TurnStart` must not duplicate it.
            AgentEvent::TurnStart => Vec::new(),
            AgentEvent::MessageStart { .. } => Vec::new(),
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event {
                pi::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                    vec![ThreadEvent::AgentText(delta.clone())]
                }
                pi::types::AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    vec![ThreadEvent::AgentThinking(delta.clone())]
                }
                _ => Vec::new(),
            },
            AgentEvent::MessageEnd { message } => match message_stop_reason(message) {
                Some(PiStopReason::Stop) => vec![ThreadEvent::Stop(ManoxStopReason::EndTurn)],
                Some(PiStopReason::Length) => vec![ThreadEvent::Stop(ManoxStopReason::MaxTokens)],
                Some(PiStopReason::ToolUse) => vec![ThreadEvent::Stop(ManoxStopReason::ToolUse)],
                Some(PiStopReason::Aborted) => {
                    vec![ThreadEvent::Stop(ManoxStopReason::Cancelled)]
                }
                Some(PiStopReason::Error) => {
                    vec![ThreadEvent::Error(anyhow::anyhow!(
                        "{}",
                        message_error_text(message)
                    ))]
                }
                None => Vec::new(),
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => vec![ThreadEvent::ToolCall {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                title: tool_title(tool_name, arguments),
                status: ToolCallStatus::Running,
                input: Some(arguments.clone()),
            }],
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                // The pi-extensions bash tool streams `{"output": chunk}`
                // partials; surface them as live tool output. Other partial
                // shapes carry no renderable text.
                match partial_result.get("output").and_then(|v| v.as_str()) {
                    Some(chunk) if !chunk.is_empty() => vec![ThreadEvent::ToolOutput {
                        id: tool_call_id.clone(),
                        chunk: chunk.to_string(),
                    }],
                    _ => Vec::new(),
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => vec![
                ThreadEvent::ToolCall {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    title: tool_name.clone(),
                    status: if *is_error {
                        ToolCallStatus::Error
                    } else {
                        ToolCallStatus::Success
                    },
                    input: None,
                },
                ThreadEvent::ToolResult {
                    id: tool_call_id.clone(),
                    output: tool_result_text(result),
                    is_error: *is_error,
                },
            ],
            AgentEvent::Retry {
                attempt,
                max_attempts,
                delay,
                reason,
                detail,
            } => vec![ThreadEvent::Retry {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_secs: delay.as_secs(),
                reason: reason.clone(),
                detail: detail.clone(),
            }],
            // Turn boundaries are owned by the facade, which knows whether the
            // run was cancelled or failed and which steers stranded.
            AgentEvent::TurnEnd { .. } => Vec::new(),
        }
    }

    /// Restore mapping: pi harness history onto the `agent::Message` history
    /// the rebuild path (`build_items`) renders. Blocks map one-to-one;
    /// terminal error/abort states surface as a trailing assistant text note
    /// so a reloaded session shows why the last run stopped.
    pub fn harness_messages_to_messages(input: &[AgentMessage]) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        for m in input {
            match m {
                AgentMessage::User { content, .. } => {
                    let content: Vec<MessageContent> = content
                        .iter()
                        .map(content_block_to_message_content)
                        .collect();
                    out.push(Message::user_with_content(content));
                }
                AgentMessage::Assistant {
                    content,
                    stop_reason,
                    error_message,
                    ..
                } => {
                    let mut blocks: Vec<MessageContent> = content
                        .iter()
                        .map(content_block_to_message_content)
                        .collect();
                    if matches!(stop_reason, Some(PiStopReason::Error)) {
                        blocks.push(MessageContent::Text(format!(
                            "[turn failed: {}]",
                            error_message.as_deref().unwrap_or("unknown error")
                        )));
                    }
                    if matches!(stop_reason, Some(PiStopReason::Aborted)) {
                        blocks.push(MessageContent::Text("[turn aborted]".to_string()));
                    }
                    out.push(Message::assistant(blocks));
                }
                AgentMessage::BashExecution {
                    command, output, ..
                } => {
                    // Dedicated shell-record card comes later; inline for now.
                    out.push(Message::assistant(vec![MessageContent::Text(format!(
                        "$ {command}\n{output}"
                    ))]));
                }
                AgentMessage::Custom {
                    content, display, ..
                } => {
                    if *display {
                        let blocks: Vec<MessageContent> = content
                            .iter()
                            .map(content_block_to_message_content)
                            .collect();
                        if !blocks.is_empty() {
                            out.push(Message::assistant(blocks));
                        }
                    }
                }
                AgentMessage::ToolResult {
                    tool_call_id,
                    tool_name,
                    content,
                    is_error,
                    ..
                } => {
                    // Tool results live in `Role::User` messages per the wire
                    // contract `build_items` expects.
                    out.push(Message::user_with_content(vec![
                        MessageContent::ToolResult(LanguageModelToolResult {
                            tool_use_id: tool_call_id.clone(),
                            tool_name: tool_name.clone().into(),
                            is_error: *is_error,
                            content: text_of_blocks(content),
                        }),
                    ]));
                }
            }
        }
        out
    }

    /// One pi content block onto one manox content block.
    fn content_block_to_message_content(block: &ContentBlock) -> MessageContent {
        match block {
            ContentBlock::Text { text, .. } => MessageContent::Text(text.clone()),
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => MessageContent::Thinking {
                text: thinking.clone(),
                signature: signature.clone(),
            },
            ContentBlock::Image { data, mime_type } => MessageContent::Image {
                data: data.clone(),
                mime_type: mime_type.clone(),
            },
            ContentBlock::ToolUse {
                id,
                name,
                input,
                thought_signature,
            } => MessageContent::ToolUse(LanguageModelToolUse {
                id: id.clone(),
                name: name.clone().into(),
                raw_input: input.to_string(),
                input: input.clone(),
                is_input_complete: true,
                thought_signature: thought_signature.clone(),
            }),
        }
    }

    /// Concatenate the text blocks of a tool result for display.
    fn tool_result_text(result: &pi::tool::AgentToolResult) -> String {
        text_of_blocks(&result.content)
    }

    fn text_of_blocks(blocks: &[ContentBlock]) -> String {
        let mut out = String::new();
        for block in blocks {
            if let ContentBlock::Text { text, .. } = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    fn message_stop_reason(message: &AgentMessage) -> Option<PiStopReason> {
        match message {
            AgentMessage::Assistant { stop_reason, .. } => *stop_reason,
            _ => None,
        }
    }

    fn message_error_text(message: &AgentMessage) -> String {
        match message {
            AgentMessage::Assistant { error_message, .. } => error_message
                .clone()
                .unwrap_or_else(|| "the pi session hit an error".to_string()),
            _ => "the pi session hit an error".to_string(),
        }
    }

    /// Human-readable tool card title from the pi tool name + arguments.
    /// Falls back to the bare name for tools without a recognized target
    /// field.
    pub fn tool_title(name: &str, args: &serde_json::Value) -> String {
        let arg = |key: &str| -> Option<String> {
            args.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match name {
            "read" | "write" | "ls" => match arg("path") {
                Some(path) => format!("{name} {path}"),
                None => name.to_string(),
            },
            "edit" | "edit_diff" => match arg("path") {
                Some(path) => format!("edit {path}"),
                None => "edit".to_string(),
            },
            "grep" => match (arg("pattern"), arg("path")) {
                (Some(pattern), Some(path)) => format!("grep {pattern} {path}"),
                (Some(pattern), None) => format!("grep {pattern}"),
                _ => "grep".to_string(),
            },
            "find" => match arg("pattern") {
                Some(pattern) => format!("find {pattern}"),
                None => "find".to_string(),
            },
            "bash" => match arg("command") {
                Some(command) => format!("$ {command}"),
                None => "bash".to_string(),
            },
            "bash_output" => "bash output".to_string(),
            "task_stop" => "stop task".to_string(),
            "agent" => match arg("subagent_type") {
                Some(kind) => format!("agent {kind}"),
                None => "agent".to_string(),
            },
            _ => name.to_string(),
        }
    }
}
