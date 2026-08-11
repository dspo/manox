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
use pi_extensions::monitor::{MonitorManager, MonitorTool};
use pi_extensions::{BackgroundRegistry, BashOutputTool, TaskStopTool};
use tokio::sync::mpsc;

use crate::db::ThreadSummary;
use crate::language_model::{MessageContent, TokenUsage};
use crate::message::Message;
use crate::permission::{PendingAuthMeta, ToolAuthorizationResponse};
use crate::pi_approval::{ApprovalGate, ApprovalGatedTool, PiAskUserQuestionTool};
use crate::thread::{ApprovalMode, ThreadEvent};
use crate::thread_engine::{BackendNotice, SpawnedEngine, ThreadEngine};

/// Commands the gpui side sends to the pi actor.
enum SessionCmd {
    /// Start a turn with the given user text and attached images.
    Prompt {
        text: String,
        images: Vec<pi::types::ContentBlock>,
    },
    /// Inject a steer into the running turn.
    Steer {
        id: String,
        text: String,
        images: Vec<pi::types::ContentBlock>,
    },
    /// Retract a queued steer.
    CancelSteer(String),
    /// Abort the running turn.
    Abort,
    /// Hot-swap the model for the next provider request.
    SetModel(PiModel),
    /// Map the reasoning effort onto pi's thinking level.
    SetThinkingLevel(Option<String>),
    /// Switch the approval policy and persist it in the session sidecar.
    SetApprovalMode(ApprovalMode),
    /// Manual compaction (`/compact`), optionally steering the summary.
    Compact { custom_instructions: Option<String> },
    /// Toggle plan mode (persisted sidecar + hooks + instruction injection).
    SetPlanMode { enabled: bool },
    /// Persist whether a plan review card is pending (restore re-surfaces it).
    SetPlanReviewPending(bool),
    /// Execute an approved plan: exit plan mode, optionally compact the
    /// planning context toward the plan file, then run the seed turn.
    ApprovePlan {
        compact: bool,
        compact_instructions: Option<String>,
        seed_text: String,
    },
    /// Re-point the session at an existing jsonl file.
    Open { path: PathBuf },
    /// Create a fresh session in the given directory, optionally bound to a
    /// project (persisted in the session sidecar).
    NewSession {
        cwd: PathBuf,
        project: Option<PathBuf>,
    },
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
    /// USD cost aggregated from the kernel's rate-card pricing (#418 wire
    /// boundary costing); 0 until the session carries priced usage.
    cumulative_cost: Mutex<f64>,
    per_model_cost: Mutex<HashMap<String, f64>>,
    /// Shared with the approval gate so `SetModel` is visible to the
    /// reviewer without a second synchronization point.
    model: Arc<Mutex<Option<PiModel>>>,
    sessions: Mutex<Vec<ThreadSummary>>,
    active_path: Mutex<Option<PathBuf>>,
    /// Plan-mode state shared by the actor, the hooks, the gate, and the
    /// `ProposePlan` tool.
    plan: Arc<crate::plan_mode::PlanSessionState>,
    /// The host approval gate wrapping every tool (mode, always-allow
    /// cache, pending UI round trips).
    gate: Arc<ApprovalGate>,
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
    project: Option<PathBuf>,
) -> SpawnedEngine {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (notice_tx, notice_rx) = mpsc::unbounded_channel();
    let model_slot = Arc::new(Mutex::new(model.clone()));
    let gate = Arc::new(ApprovalGate::new(
        notice_tx.clone(),
        Arc::clone(&model_slot),
    ));
    let state = Arc::new(EngineState {
        running: AtomicBool::new(false),
        history: Mutex::new(Vec::new()),
        request_usage: Mutex::new(HashMap::new()),
        cumulative: Mutex::new(TokenUsage::default()),
        per_model: Mutex::new(HashMap::new()),
        cumulative_cost: Mutex::new(0.0),
        per_model_cost: Mutex::new(HashMap::new()),
        model: model_slot,
        sessions: Mutex::new(Vec::new()),
        active_path: Mutex::new(initial_path.clone()),
        gate,
        plan: crate::plan_mode::PlanSessionState::new(),
    });
    crate::runtime::handle().spawn(run_actor(
        cwd,
        model,
        sessions_dir,
        initial_path.clone(),
        fresh,
        project,
        cmd_rx,
        notice_tx.clone(),
        Arc::clone(&state),
    ));
    // Display-only streaming preview: while the actor's eager restore reads
    // the whole session file, stream its transcript into the mirrored history
    // in batches so the workspace paints the first messages early. The
    // authoritative `sync_history` at `Ready` replaces the preview.
    if let Some(path) = initial_path {
        spawn_history_preview(path, Arc::clone(&state), notice_tx);
    }
    SpawnedEngine {
        engine: Arc::new(PiEngine { cmd_tx, state }),
        events: notice_rx,
    }
}

/// Stream a session file's transcript into `state.history` as display-only
/// preview batches for the workspace while the actor's eager restore runs in
/// parallel. The extension's lazy reader yields entries in append order; each
/// batch appends its mapped `Message`s to the mirrored history and notifies
/// the facade (`HistoryProgress`). The authoritative `sync_history` at
/// `Ready` replaces the mirror; the length guard below stops the drain once
/// that happened (appending past the authoritative list would clobber it).
fn spawn_history_preview(
    path: PathBuf,
    state: Arc<EngineState>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
) {
    crate::runtime::handle().spawn(async move {
        let mut stream = match pi_extensions::session_stream::SessionTranscriptStream::open(&path)
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                // The preview degrades to the authoritative restore (which
                // reports its own error); surface why the display stream
                // never started so a silent no-preview is diagnosable.
                tracing::warn!(error = %err, path = %path.display(), "history preview open failed");
                return;
            }
        };
        let mut expected = 0usize;
        while let Some(entries) = stream.next_batch(32, 256 * 1024).await {
            if entries.is_empty() {
                continue;
            }
            let msgs: Vec<Message> = entries
                .iter()
                .flat_map(pi::session::session_entry_to_context_messages)
                .flat_map(|m| adapt::harness_messages_to_messages(std::slice::from_ref(&m)))
                .collect();
            if msgs.is_empty() {
                continue;
            }
            let mut history = state.history.lock().unwrap();
            // Only the preview writer appends before `Ready`; once the
            // authoritative sync replaced the mirror this guard fails and the
            // drain stops (the file is fully drained anyway).
            if history.len() != expected {
                break;
            }
            history.extend(msgs);
            expected = history.len();
            drop(history);
            if notice_tx.send(BackendNotice::HistoryProgress).is_err() {
                // The facade (thread entity) is gone; stop streaming.
                break;
            }
        }
    });
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

    fn cumulative_cost(&self) -> f64 {
        *self.state.cumulative_cost.lock().unwrap()
    }

    fn per_model_cost(&self) -> HashMap<String, f64> {
        self.state.per_model_cost.lock().unwrap().clone()
    }

    fn model(&self) -> Option<PiModel> {
        self.state.model.lock().unwrap().clone()
    }

    fn run(&self, prompt: String, images: Vec<pi::types::ContentBlock>) {
        let _ = self.cmd_tx.send(SessionCmd::Prompt {
            text: prompt,
            images,
        });
    }

    fn steer(&self, text: String, images: Vec<pi::types::ContentBlock>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let _ = self.cmd_tx.send(SessionCmd::Steer {
            id: id.clone(),
            text,
            images,
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

    fn set_approval_mode(&self, mode: ApprovalMode) {
        let _ = self.cmd_tx.send(SessionCmd::SetApprovalMode(mode));
    }

    fn set_plan_mode(&self, enabled: bool) {
        let _ = self.cmd_tx.send(SessionCmd::SetPlanMode { enabled });
    }

    fn set_plan_review_pending(&self, pending: bool) {
        let _ = self.cmd_tx.send(SessionCmd::SetPlanReviewPending(pending));
    }

    fn approve_plan(&self, compact: bool, compact_instructions: Option<String>, seed_text: String) {
        let _ = self.cmd_tx.send(SessionCmd::ApprovePlan {
            compact,
            compact_instructions,
            seed_text,
        });
    }

    fn compact(&self, custom_instructions: Option<String>) {
        let _ = self.cmd_tx.send(SessionCmd::Compact {
            custom_instructions,
        });
    }

    fn respond_tool_authorization(&self, id: &str, response: ToolAuthorizationResponse) {
        self.state.gate.respond(id, response);
    }

    fn pending_auth_entries(&self) -> Vec<(String, PendingAuthMeta)> {
        self.state.gate.pending_entries()
    }

    fn set_thinking_level(&self, level: Option<String>) {
        let _ = self.cmd_tx.send(SessionCmd::SetThinkingLevel(level));
    }

    fn open_session(&self, path: PathBuf) {
        let _ = self.cmd_tx.send(SessionCmd::Open { path });
    }

    fn new_session(&self, cwd: PathBuf, project: Option<PathBuf>) {
        let _ = self.cmd_tx.send(SessionCmd::NewSession { cwd, project });
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
    let mut prompt = format!(
        "You are Manox Pi, a coding agent running inside the manox app on the pi harness.\n\
         Working directory: {cwd}\n\
         Date: {date}\n\n\
         Use your tools to inspect, edit, and create files and to run shell commands.\n\
         Make changes directly, keep replies concise, and verify your work when practical.",
        cwd = cwd.display(),
    );
    // Skill summaries let the model know which skills are installed (users
    // invoke them via `/name` slash commands). Parity with the retired manox
    // system prompt, which rendered `skill::summaries_or_empty()` into its
    // template; the pi path has no skill tool, so the wording only promises
    // what exists.
    let summaries = crate::skill::summaries_or_empty();
    if !summaries.is_empty() {
        prompt.push_str("\n\n## Available skills\n");
        prompt.push_str("Installed skills, invocable by the user as `/name` slash commands:\n");
        for s in &summaries {
            prompt.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
    }
    prompt
}

/// The full pi toolset: pi's file tools plus the pi-extensions bash/sub-agent
/// orchestration (assembly mirrors the `pi-extensions` orchestration example).
/// Every tool rides behind the host's [`ApprovalGatedTool`] (the kernel ships
/// no gate — approval policy is a harness concern); `AskUserQuestion` joins
/// ungated because asking the user is itself the interaction.
///
/// Returns the tools plus the session-scoped orchestrators that must attach
/// once the session exists (their steerers and lifecycle hooks need a live
/// session handle).
fn build_tools(
    cwd: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) -> (Vec<Arc<dyn PiAgentTool>>, SessionOrchestrators) {
    let background = Arc::new(BackgroundRegistry::new());
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let monitor = Arc::new(MonitorManager::new(Arc::clone(&background)));
    let bash = BashTool::new(
        Arc::new(PersistentShellOperations::new(cwd)),
        background.clone(),
    )
    .with_manager(Arc::clone(&manager));

    let tools: Vec<Arc<dyn PiAgentTool>> = vec![
        Arc::new(pi::tools::read::ReadTool),
        Arc::new(pi::tools::write::WriteTool),
        Arc::new(pi::tools::edit::EditTool),
        Arc::new(pi::tools::grep::GrepTool),
        Arc::new(pi::tools::find::FindTool),
        Arc::new(pi::tools::ls::LsTool),
        Arc::new(bash),
        Arc::new(MonitorTool::new(Arc::clone(&monitor))),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(TaskStopTool::new(background).with_ws_registry(monitor.ws_registry())),
    ];
    // Plan-mode gate exemption: plan-file writes stay approval-free while
    // plan mode is active (the `ToolCall` hook blocks everything else).
    let plan_policy = Arc::new(crate::plan_mode::PlanGatePolicy {
        state: Arc::clone(plan),
        plans_dir: crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans")),
        cwd: cwd.to_path_buf(),
    });
    let mut tools: Vec<Arc<dyn PiAgentTool>> = tools
        .into_iter()
        .map(|tool| {
            Arc::new(
                ApprovalGatedTool::new(tool, Arc::clone(gate))
                    .with_plan_policy(Arc::clone(&plan_policy)),
            ) as Arc<dyn PiAgentTool>
        })
        .collect();
    tools.push(Arc::new(PiAskUserQuestionTool::new(Arc::clone(gate))));
    // Plan proposal rides ungated like AskUserQuestion: submitting a plan is
    // the verdict request itself, not a side effect.
    tools.push(Arc::new(crate::plan_mode::ProposePlanTool::new(
        notice_tx.clone(),
        Arc::clone(plan),
        plan_policy.plans_dir.clone(),
    )));
    // Execution progress: the model publishes its task list; the snapshot
    // rides PlanUpdated to the context rail. Ungated (mutates nothing on
    // disk); plan mode's ToolCall hook blocks it while planning.
    tools.push(Arc::new(crate::plan::UpdatePlanTool::new(
        notice_tx.clone(),
    )));
    // MCP servers (mcp.toml + plugin .mcp.json): each advertised tool rides
    // behind the same approval gate as built-ins (remote calls are mutating
    // by default). A registry that never initialized (pre-`agent::init`
    // tests) contributes nothing.
    if let Some(registry) = crate::mcp::try_global() {
        for server in registry.servers() {
            for tool in &server.tools {
                let mcp_tool = Arc::new(crate::mcp::pi_tool::PiMcpTool::new(
                    server.name.clone(),
                    tool.clone(),
                    Arc::clone(&server.client),
                ));
                tools.push(Arc::new(ApprovalGatedTool::new(mcp_tool, Arc::clone(gate))));
            }
        }
    }
    // The sub-agent tool needs a concrete model; a session assembled before
    // registration landed (first seconds after launch) skips it.
    if let Some(model) = model {
        let mut registry = AgentRegistry::new();
        register_defaults(&mut registry);
        // User-authored (~/.config/cx/manox/agents) + plugin-provided
        // (`<plugin>/agents/`, namespaced) definitions layer over the
        // built-ins; same-name user files override built-ins.
        crate::agent_defs::register_user_and_plugin(&mut registry);
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
    (
        tools,
        SessionOrchestrators {
            monitor,
            background: manager,
        },
    )
}

/// Session-scoped orchestrators that attach once a session exists: the
/// monitor manager (background command / WebSocket monitors) and the bash
/// background manager. Both are held by the session's tools as well, so the
/// managers live exactly as long as their session — a replaced session drops
/// its tools, and the monitor manager's `Drop` stops every monitor.
struct SessionOrchestrators {
    monitor: Arc<MonitorManager>,
    background: Arc<BackgroundManager>,
}

/// Bind the orchestrators to a freshly built session: the monitor steerer
/// lands events in the session's steering queue, and the background manager
/// subscribes to the session's lifecycle.
fn attach_orchestrators(session: &mut AgentSession, orch: &SessionOrchestrators) {
    let handle = session.handle();
    orch.monitor.attach(&handle);
    orch.background.attach(session);
}

fn steer_message(text: String, images: Vec<ContentBlock>) -> AgentMessage {
    let mut content = vec![ContentBlock::Text {
        text,
        signature: None,
    }];
    // TS `createUserMessage(text, images)` parity: image blocks ride the
    // steered user message behind the text.
    content.extend(images);
    AgentMessage::User {
        content,
        timestamp: chrono::Utc::now(),
    }
}

/// Drive one session run to completion while still servicing mid-run
/// commands (abort/steer/cancel/shutdown) through the session handle.
/// Shared by user prompts and monitor idle-wakeups. Returns the run result
/// and whether an abort was requested.
async fn drive_run<F>(
    run: F,
    handle: &pi::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
) -> (anyhow::Result<Vec<AgentMessage>>, bool)
where
    F: std::future::Future<Output = anyhow::Result<Vec<AgentMessage>>>,
{
    tokio::pin!(run);
    let mut abort_requested = false;
    let mut channel_open = true;
    let result = loop {
        if !channel_open {
            break run.await;
        }
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                Some(SessionCmd::Abort) => {
                    abort_requested = true;
                    handle.abort();
                }
                Some(SessionCmd::Steer { id, text, images }) => {
                    handle.steer(steer_message(text, images));
                    run_steers.push(id);
                }
                Some(SessionCmd::CancelSteer(id)) => {
                    handle.cancel_steer(&id);
                }
                Some(SessionCmd::Shutdown) => *shutdown_after_run = true,
                Some(_) => {} // queued prompts/reconfigs wait for settle
                None => {
                    // Facade dropped mid-run: abort, settle, exit.
                    channel_open = false;
                    *shutdown_after_run = true;
                    if !abort_requested {
                        abort_requested = true;
                        handle.abort();
                    }
                }
            },
            result = &mut run => break result,
        }
    };
    (result, abort_requested)
}

/// Post-run settlement shared by user prompts and monitor idle-wakeups:
/// error notice, running flag, history/usage/session-list mirrors, steer
/// accounting, title eligibility, and the `Settled` notice.
#[allow(clippy::too_many_arguments)] // settlement plumbing: each input is a distinct sink
async fn settle_run(
    result: &anyhow::Result<Vec<AgentMessage>>,
    abort_requested: bool,
    session: &AgentSession,
    state: &Arc<EngineState>,
    repo: &pi::session::repository::SessionRepository,
    title_state: &Arc<Mutex<TitleState>>,
    runtime: &ModelRuntime,
    pi_model: &PiModel,
    sessions_dir: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    run_steers: &mut Vec<String>,
) {
    let failed = result.is_err();
    if let Err(err) = result {
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
            anyhow::anyhow!("{err:#}"),
        ))));
    }
    state.running.store(false, Ordering::Relaxed);
    sync_history(session, sessions_dir, state).await;
    sync_usage(session, state).await;
    refresh_session_list(repo, state).await;
    let (steered, stranded) = if abort_requested || failed {
        (Vec::new(), std::mem::take(run_steers))
    } else {
        (std::mem::take(run_steers), Vec::new())
    };
    // A natural terminal turn may earn (or re-earn) the LLM title;
    // cancelled/failed turns keep the interim summary.
    if !abort_requested && !failed {
        maybe_generate_title(
            title_state,
            runtime,
            pi_model,
            session,
            sessions_dir,
            notice_tx,
        );
    }
    let _ = notice_tx.send(BackendNotice::Settled {
        cancelled: abort_requested,
        failed,
        steered,
        stranded,
    });
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
            // The user entry lands in the transcript right after the first
            // TurnStart; its MessageEnd is the earliest reliable "the
            // conversation now exists" signal for the sidebar.
            if let AgentEvent::MessageEnd { message } = &event
                && matches!(**message, AgentMessage::User { .. })
            {
                let _ = tx.send(BackendNotice::SessionListDirty);
            }
            for te in adapt::agent_event_to_thread_events(&event) {
                let _ = tx.send(BackendNotice::Event(Box::new(te)));
            }
        })
    }))
}

/// Adapt harness lifecycle events onto the notice channel. Carries the
/// compaction visibility pair (TS `compaction_start` / `compaction_end`):
/// start flips the UI into its summarizing state, a successful end lands the
/// Recap card. The end event's token counts ride the result; the UI chrome
/// consumes only the summary.
fn subscribe_harness_events(
    session: &mut AgentSession,
    sessions_dir: PathBuf,
    session_path: PathBuf,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    wakeup_tx: &mpsc::UnboundedSender<()>,
) -> pi::harness::HarnessSubscription {
    let tx = notice_tx.clone();
    let wake = wakeup_tx.clone();
    session.subscribe_harness(Arc::new(move |event| match event {
        // Idle-wakeup signal: a monitor steered events into the queue. The
        // actor decides whether the session is idle and resumes it
        // (`continue_` drains the steering queue first) — the listener stays
        // stateless and never touches the session itself.
        pi::harness::HarnessEvent::QueueUpdate { steer, .. } if steer > 0 => {
            let _ = wake.send(());
        }
        pi::harness::HarnessEvent::CompactionStart { .. } => {
            let _ = tx.send(BackendNotice::Event(Box::new(
                ThreadEvent::CompactionStarted { tokens_before: 0 },
            )));
        }
        pi::harness::HarnessEvent::CompactionEnd {
            result: Some(result),
            aborted: false,
            ..
        } => {
            // The transcript was rebuilt as a summary user message that
            // consumes a display ordinal, so the sidecar's registry display
            // forms no longer align; drop them so a reload never mislabels a
            // prompt. Fire-and-forget: the manual `/compact` path awaits the
            // same clear before mirroring.
            clear_registry_displays_spawn(sessions_dir.clone(), session_path.clone());
            let _ = tx.send(BackendNotice::Event(Box::new(ThreadEvent::Compaction {
                summary: result.summary,
                messages_compacted: 0,
                tokens_before: result.tokens_before,
            })));
        }
        _ => {}
    }))
}

/// Build the session builder against the given project dir, using the shared
/// runtime and model.
fn session_builder(
    cwd: &Path,
    sessions_dir: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) -> (pi::coding_agent::AgentSessionBuilder, SessionOrchestrators) {
    let (tools, orchestrators) = build_tools(cwd, runtime, model, gate, plan, notice_tx);
    let mut builder = create_agent_session()
        .with_cwd(cwd.to_path_buf())
        .with_session_dir(sessions_dir.to_path_buf())
        .with_model_runtime(runtime.clone())
        .with_system_prompt(system_prompt(cwd))
        .with_tools(tools);
    if let Some(model) = model {
        builder = builder.with_model(model.clone());
    }
    (builder, orchestrators)
}

/// Register plan-mode extension hooks on a freshly built/restored session:
/// `BeforeAgentStart` injects the rendered plan-mode instructions every turn
/// while active; `ToolCall` enforces the read-only guarantee (plan-file
/// writes excepted). Both read through the shared [`PlanSessionState`].
fn attach_plan_hooks(
    session: &mut AgentSession,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    cwd: &Path,
) {
    session.on(
        pi::harness::HookPoint::BeforeAgentStart,
        crate::plan_mode::injection_handler(Arc::clone(plan)),
    );
    let plans_dir = crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans"));
    session.on(
        pi::harness::HookPoint::ToolCall,
        crate::plan_mode::gate_handler(Arc::clone(plan), plans_dir, cwd.to_path_buf()),
    );
}

#[allow(clippy::too_many_arguments)] // actor entry: startup options stay explicit
async fn run_actor(
    cwd: PathBuf,
    model: Option<PiModel>,
    sessions_dir: PathBuf,
    initial_path: Option<PathBuf>,
    fresh: bool,
    project: Option<PathBuf>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    state: Arc<EngineState>,
) {
    // Session assembly preflights the model against the registry, so resolve
    // only after the one-shot background registration (parallelized per
    // provider, sub-second) has landed. The snapshot must be fetched AFTER
    // the wait: `global()` clones the current Arc, and the init thread
    // swaps it once registration completes — an early handle stays empty.
    crate::pi_providers::wait_ready().await;
    let registry = crate::pi_providers::global();
    let runtime = ModelRuntime::with_provider_registry(registry.clone()).with_catalog(Arc::new(
        crate::pi_providers::LegacyAliasCatalog::new(registry.clone()),
    ));
    // Reviewer side calls resolve their stream through this runtime.
    state.gate.set_runtime(runtime.clone());
    let Some(mut pi_model) = model.or_else(crate::pi_providers::default_model) else {
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
        let (builder, orchestrators) = session_builder(
            &tool_cwd,
            &sessions_dir,
            &runtime,
            Some(&pi_model),
            &state.gate,
            &state.plan,
            &notice_tx,
        );
        match builder.open(info.path).await {
            Ok(mut s) => {
                attach_orchestrators(&mut s, &orchestrators);
                attach_plan_hooks(&mut s, &state.plan, &tool_cwd);
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
            let (builder, orchestrators) = session_builder(
                &cwd,
                &sessions_dir,
                &runtime,
                Some(&pi_model),
                &state.gate,
                &state.plan,
                &notice_tx,
            );
            match builder.build().await {
                Ok(mut s) => {
                    attach_orchestrators(&mut s, &orchestrators);
                    attach_plan_hooks(&mut s, &state.plan, &cwd);
                    s
                }
                Err(err) => {
                    // Self-diagnosing failure: name what the registry held at
                    // build time so startup reports are actionable.
                    let registered = registry.provider_names();
                    tracing::error!(
                        error = %err,
                        model_provider = %pi_model.provider,
                        registered = ?registered,
                        "pi session build failed"
                    );
                    let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                        "pi session build failed: {err} (registered providers: {registered:?})"
                    )));
                    return;
                }
            }
        }
    };
    *state.active_path.lock().unwrap() = Some(session.path().to_path_buf());
    if let Some(project) = &project {
        write_project_sidecar(&sessions_dir, session.path(), project).await;
    }
    refresh_session_list(&repo, &state).await;

    // Idle-wakeup channel: the harness listener signals when monitor events
    // land in the steering queue; the actor resumes an idle session below.
    let (wakeup_tx, mut wakeup_rx) = mpsc::unbounded_channel::<()>();

    // Stream run events back to the gpui drainer. Re-registered after a
    // session rebuild (listeners live on the old Agent).
    let session_path = session.path().to_path_buf();
    let mut _subscription = subscribe_session(&session, &notice_tx);
    let mut _harness_subscription = subscribe_harness_events(
        &mut session,
        sessions_dir.clone(),
        session_path,
        &notice_tx,
        &wakeup_tx,
    );

    // The approval mode rides the session sidecar: restore it so a
    // reopened Danger session doesn't silently gate (or vice versa).
    let approval_mode = load_approval_mode(&sessions_dir, session.path()).await;
    state.gate.set_mode(approval_mode);
    // Plan mode rides the same sidecar: restore the flag so a reopened
    // planning session keeps its read-only gate; the facade re-renders and
    // re-sends the instructions once it sees `Ready`.
    let (plan_mode_restored, plan_file_restored) =
        load_plan_state(&sessions_dir, session.path()).await;
    if plan_mode_restored {
        state.plan.set(true, plan_file_restored.clone());
        state
            .plan
            .set_active_instructions(render_plan_instructions());
    }
    let plan_review_pending = load_plan_review_pending(&sessions_dir, session.path()).await;
    let mut title_state = load_title_state(&sessions_dir, session.path(), &session).await;

    // Mirror the authoritative transcript BEFORE `Ready` is sent: the
    // facade's Ready handler reads `history()` immediately, and a drainer
    // that woke first would rebuild from a stale (empty or preview-only)
    // mirror and strand the thread on the loading screen. Unconditional so a
    // failed restore (fresh fallback session) also clears any preview the
    // display stream had written.
    sync_history(&session, &sessions_dir, &state).await;
    if restored {
        sync_usage(&session, &state).await;
    }
    let _ = notice_tx.send(BackendNotice::Ready {
        restored,
        model: Some(pi_model.clone()),
        approval_mode,
        plan_mode: plan_mode_restored,
        plan_file: plan_file_restored,
        plan_review_pending,
    });

    let mut run_steers: Vec<String> = Vec::new();
    let mut shutdown_after_run = false;

    loop {
        // Between runs the actor wakes on either a facade command or a
        // monitor idle-wakeup (steered events queued while the session was
        // idle). Mid-run wakeups simply accumulate and are re-checked after
        // settlement.
        let cmd = tokio::select! {
            // None = facade dropped: shut down.
            cmd = cmd_rx.recv() => cmd,
            _ = wakeup_rx.recv() => {
                // Collapse wakeups queued while the actor was busy; the
                // steering-queue check below decides whether a run is owed.
                while wakeup_rx.try_recv().is_ok() {}
                if !session.steering_messages().is_empty() {
                    // Idle wakeup — the Rust equivalent of TS Pi's
                    // `sendUserMessage` idle semantics: a monitor steered
                    // events while the session was idle, so resume the run;
                    // `continue_` drains the steering queue first.
                    state.running.store(true, Ordering::Relaxed);
                    let handle = session.handle();
                    let (result, abort_requested) = drive_run(
                        session.continue_(),
                        &handle,
                        &mut cmd_rx,
                        &mut run_steers,
                        &mut shutdown_after_run,
                    )
                    .await;
                    settle_run(
                        &result,
                        abort_requested,
                        &session,
                        &state,
                        &repo,
                        &title_state,
                        &runtime,
                        &pi_model,
                        &sessions_dir,
                        &notice_tx,
                        &mut run_steers,
                    )
                    .await;
                    if shutdown_after_run {
                        break;
                    }
                }
                continue;
            }
        };
        let Some(cmd) = cmd else { break };
        match cmd {
            SessionCmd::Prompt { text, images } => {
                state.running.store(true, Ordering::Relaxed);
                let handle = session.handle();
                // Drive the run while still servicing mid-run commands
                // (abort/steer) through the session handle.
                let (result, abort_requested) = drive_run(
                    session.prompt_with_images(&text, images),
                    &handle,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
                    &repo,
                    &title_state,
                    &runtime,
                    &pi_model,
                    &sessions_dir,
                    &notice_tx,
                    &mut run_steers,
                )
                .await;
                if shutdown_after_run {
                    break;
                }
            }
            SessionCmd::Steer { id, text, images } => {
                // A steer queued while idle is injected into the next turn;
                // confirmation (SteerInjected) rides that turn's settlement.
                session.handle().steer(steer_message(text, images));
                run_steers.push(id);
            }
            SessionCmd::CancelSteer(id) => {
                session.handle().cancel_steer(&id);
            }
            SessionCmd::Abort => {
                session.abort();
            }
            SessionCmd::SetModel(new_model) => {
                // Streams dispatch by `model.provider` through the shared
                // registry, so a cross-provider switch reaches the right
                // endpoint + credential (the old bridge captured the
                // initial model's credential for every later model).
                if let Err(err) = session.set_model(new_model.clone()).await {
                    tracing::warn!("pi set_model failed: {err}");
                }
                // Keep the actor's working model in sync: Open/NewSession
                // below build sessions with it.
                pi_model = new_model.clone();
                *state.model.lock().unwrap() = Some(new_model);
            }
            SessionCmd::SetApprovalMode(mode) => {
                state.gate.set_mode(mode);
                if let Err(err) =
                    write_approval_mode_sidecar(&sessions_dir, session.path(), mode).await
                {
                    tracing::warn!(error = %err, "failed to persist approval mode");
                }
            }
            SessionCmd::SetPlanMode { enabled } => {
                let plan_file = state.plan.plan_file();
                state.plan.set(enabled, plan_file);
                state
                    .plan
                    .set_active_instructions(enabled.then(render_plan_instructions).flatten());
                if let Err(err) =
                    write_plan_sidecar(&sessions_dir, session.path(), &state.plan).await
                {
                    tracing::warn!(error = %err, "failed to persist plan mode");
                }
                let _ = notice_tx.send(BackendNotice::Event(Box::new(
                    ThreadEvent::PlanModeChanged { enabled },
                )));
            }
            SessionCmd::SetPlanReviewPending(pending) => {
                if let Err(err) =
                    write_plan_review_pending_sidecar(&sessions_dir, session.path(), pending).await
                {
                    tracing::warn!(error = %err, "failed to persist plan review pending flag");
                }
            }
            SessionCmd::ApprovePlan {
                compact,
                compact_instructions,
                seed_text,
            } => {
                // Exit plan mode first: the execution turn runs with full
                // tool access (the hook + gate read the shared state).
                let plan_file = state.plan.plan_file();
                state.plan.set(false, plan_file);
                state.plan.set_active_instructions(None);
                if let Err(err) =
                    write_plan_sidecar(&sessions_dir, session.path(), &state.plan).await
                {
                    tracing::warn!(error = %err, "failed to persist plan-mode exit");
                }
                let _ = notice_tx.send(BackendNotice::Event(Box::new(
                    ThreadEvent::PlanModeChanged { enabled: false },
                )));
                if compact {
                    match session.compact(compact_instructions.as_deref()).await {
                        Ok(_) => {
                            sync_history(&session, &sessions_dir, &state).await;
                            sync_usage(&session, &state).await;
                            refresh_session_list(&repo, &state).await;
                        }
                        Err(err) => {
                            // Execute anyway — approval intent stands; the
                            // context simply keeps the planning discussion.
                            tracing::warn!(
                                error = %err,
                                "plan-approval compaction failed; executing without compaction"
                            );
                        }
                    }
                }
                state.running.store(true, Ordering::Relaxed);
                let handle = session.handle();
                let (result, abort_requested) = drive_run(
                    session.prompt(&seed_text),
                    &handle,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
                    &repo,
                    &title_state,
                    &runtime,
                    &pi_model,
                    &sessions_dir,
                    &notice_tx,
                    &mut run_steers,
                )
                .await;
            }
            SessionCmd::Compact {
                custom_instructions,
            } => {
                // The kernel compacts an idle transcript only; the facade
                // already drops `/compact` while a turn runs, so a queued
                // command arriving here settles first by construction.
                match session.compact(custom_instructions.as_deref()).await {
                    Ok(_) => {
                        // The transcript was rebuilt and the summarization
                        // call consumed tokens — re-mirror both, and the
                        // session list (the summary row may have changed).
                        // Await the display-form clear: the mirror below would
                        // otherwise attach the pre-compaction ordinals to the
                        // wrong prompts.
                        clear_registry_displays(&sessions_dir, session.path()).await;
                        sync_history(&session, &sessions_dir, &state).await;
                        sync_usage(&session, &state).await;
                        refresh_session_list(&repo, &state).await;
                    }
                    Err(err)
                        if err
                            .downcast_ref::<pi::compaction::NothingToCompact>()
                            .is_some() =>
                    {
                        tracing::debug!("pi compact: nothing to compact");
                    }
                    Err(err) => {
                        let _ =
                            notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(err))));
                    }
                }
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
                    &state.gate,
                    &state.plan,
                )
                .await;
                _subscription = subscribe_session(&session, &notice_tx);
                _harness_subscription = subscribe_harness_events(
                    &mut session,
                    sessions_dir.to_path_buf(),
                    path.to_path_buf(),
                    &notice_tx,
                    &wakeup_tx,
                );
                resync_plan_state(&sessions_dir, &path, &state.plan, &notice_tx).await;
                *state.active_path.lock().unwrap() = Some(path);
                resync_approval_mode(&session, &sessions_dir, &state, &notice_tx).await;
                title_state = load_title_state(&sessions_dir, session.path(), &session).await;
                sync_history(&session, &sessions_dir, &state).await;
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
            }
            SessionCmd::NewSession { cwd, project } => {
                let (builder, orchestrators) = session_builder(
                    &cwd,
                    &sessions_dir,
                    &runtime,
                    Some(&pi_model),
                    &state.gate,
                    &state.plan,
                    &notice_tx,
                );
                match builder.build().await {
                    Ok(mut s) => {
                        attach_orchestrators(&mut s, &orchestrators);
                        attach_plan_hooks(&mut s, &state.plan, &cwd);
                        // A fresh session never inherits plan mode — clear
                        // any state left over from the previous session.
                        state.plan.set(false, None);
                        state.plan.set_active_instructions(None);
                        session = s;
                        let new_path = session.path().to_path_buf();
                        _subscription = subscribe_session(&session, &notice_tx);
                        _harness_subscription = subscribe_harness_events(
                            &mut session,
                            sessions_dir.clone(),
                            new_path,
                            &notice_tx,
                            &wakeup_tx,
                        );
                        *state.active_path.lock().unwrap() = Some(session.path().to_path_buf());
                        if let Some(project) = &project {
                            write_project_sidecar(&sessions_dir, session.path(), project).await;
                        }
                        resync_approval_mode(&session, &sessions_dir, &state, &notice_tx).await;
                        title_state = Arc::new(Mutex::new(TitleState::default()));
                        sync_history(&session, &sessions_dir, &state).await;
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
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
async fn rebuild_session(
    session: &mut AgentSession,
    path: &Path,
    sessions_dir: &Path,
    runtime: &ModelRuntime,
    model: &PiModel,
    fallback_cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
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
    let (builder, orchestrators) = session_builder(
        &cwd,
        sessions_dir,
        runtime,
        Some(model),
        gate,
        plan,
        notice_tx,
    );
    match builder.open(path.to_path_buf()).await {
        Ok(mut s) => {
            attach_orchestrators(&mut s, &orchestrators);
            attach_plan_hooks(&mut s, plan, &cwd);
            *session = s;
        }
        Err(err) => {
            let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                "pi session open failed: {err}"
            )));
        }
    }
}

/// Actor-local title-generation state (manox `TitleState` parity): the
/// LLM title, the cadence anchor (user count at last evaluation), and the
/// in-flight lock. Shared with the spawned title task via `Arc<Mutex<_>>`.
#[derive(Default)]
struct TitleState {
    title: Option<String>,
    last_eval_user_count: Option<usize>,
    in_flight: bool,
}

/// Restore the title state from the session sidecar. `last_eval_user_count`
/// is derived from whether a title already exists, so a reloaded session
/// continues the cadence without re-evaluating immediately (manox parity).
async fn load_title_state(
    sessions_dir: &Path,
    session_path: &Path,
    session: &AgentSession,
) -> Arc<Mutex<TitleState>> {
    let meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .ok();
    let title = meta.and_then(|m| m.title).filter(|t| !t.trim().is_empty());
    let last_eval_user_count = title
        .is_some()
        .then(|| crate::title::count_user_messages(session.harness_messages()));
    Arc::new(Mutex::new(TitleState {
        title,
        last_eval_user_count,
        in_flight: false,
    }))
}

/// Maybe kick off an LLM title stream after a settled turn (manox
/// `maybe_generate_title` semantics): first title as soon as an assistant
/// reply exists, topic-shift re-eval on the cadence thereafter. The stream
/// runs in a spawned task (runtime resolver + `StreamFn`, reviewer-style)
/// and persists a landed title to the session sidecar.
fn maybe_generate_title(
    title_state: &Arc<Mutex<TitleState>>,
    runtime: &ModelRuntime,
    model: &PiModel,
    session: &AgentSession,
    sessions_dir: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    if !crate::settings::side_calls().title_policy().enabled {
        return;
    }
    let lang = crate::settings::load().resolve().agent;
    let (convo, user_count) = {
        let state = title_state.lock().unwrap();
        if state.in_flight {
            return;
        }
        let messages = session.harness_messages();
        let user_count = crate::title::count_user_messages(messages);
        if state.last_eval_user_count == Some(user_count) {
            return;
        }
        if state.title.is_some() && !crate::title::should_retitle(user_count) {
            return;
        }
        let Some(convo) =
            crate::title::build_title_messages(messages, state.title.as_deref(), lang)
        else {
            return;
        };
        (convo, user_count)
    };
    {
        let mut state = title_state.lock().unwrap();
        state.in_flight = true;
        state.last_eval_user_count = Some(user_count);
    }
    let runtime = runtime.clone();
    let model = model.clone();
    let sessions_dir = sessions_dir.to_path_buf();
    let session_path = session.path().to_path_buf();
    let tx = notice_tx.clone();
    let state = Arc::clone(title_state);
    crate::runtime::handle().spawn(async move {
        let result = crate::title::stream_title(&runtime, &model, convo).await;
        // Surface every outcome (manox parity): failures used to vanish into
        // a swallowed `if let Ok`, leaving the mechanical fallback with no
        // trace in the logs.
        match &result {
            Ok(title) if title.is_empty() => {
                tracing::warn!("title generation produced no usable text")
            }
            Ok(title) if crate::title::is_unchanged(title) => {
                tracing::debug!("title unchanged by model")
            }
            Ok(title) => tracing::debug!(title = %title, "title updated"),
            Err(e) => tracing::warn!(error = %format!("{e:?}"), "title generation stream failed"),
        }
        // Resolve the adoption under the lock, then persist outside it —
        // the guard must not span the sidecar awaits.
        let adopted = {
            let mut state = state.lock().unwrap();
            state.in_flight = false;
            let adopted = matches!(&result, Ok(title)
                if !title.is_empty() && !crate::title::is_unchanged(title));
            if adopted {
                state.title = result.as_ref().ok().cloned();
            }
            adopted
        };
        if adopted {
            let title = result.unwrap_or_default();
            let mut meta = pi_extensions::session_meta::load(&sessions_dir, &session_path)
                .await
                .unwrap_or_default();
            meta.title = Some(title);
            if let Err(err) =
                pi_extensions::session_meta::save(&sessions_dir, &session_path, &meta).await
            {
                tracing::warn!(error = %err, "failed to persist session title");
            }
            let _ = tx.send(BackendNotice::SessionListDirty);
        }
    });
}

/// The approval mode persisted in a session's sidecar; fresh sessions
/// (missing sidecar or field) default to AutoPilot.
async fn load_approval_mode(sessions_dir: &Path, session_path: &Path) -> ApprovalMode {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => meta
            .approval_mode
            .as_deref()
            .and_then(|raw| serde_json::from_value(serde_json::Value::String(raw.to_string())).ok())
            .unwrap_or_default(),
        Err(_) => ApprovalMode::default(),
    }
}

/// Persist the approval mode in the session sidecar so the session reopens
/// with the same gate policy.
/// Render the plan-mode-active instructions for the configured agent
/// language (the actor renders them itself — language comes from settings,
/// so no facade round-trip is needed on restore or session switches).
fn render_plan_instructions() -> Option<String> {
    let plans_dir = crate::paths::plans_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".manox/plans".to_string());
    let lang = crate::settings::load().resolve().agent;
    match crate::collaboration_mode::render_plan_mode_active(lang, &plans_dir) {
        Ok(text) => Some(text),
        Err(err) => {
            tracing::warn!(error = %err, "failed to render plan-mode instructions");
            None
        }
    }
}

async fn load_plan_state(sessions_dir: &Path, session_path: &Path) -> (bool, Option<String>) {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => (meta.plan_mode.unwrap_or(false), meta.plan_file),
        Err(_) => (false, None),
    }
}

/// Persist plan mode + last plan file from the shared state into the session
/// sidecar (`plan_mode` stored only while on; `plan_file` kept across exits
/// for the execution handoff).
async fn write_plan_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    plan: &crate::plan_mode::PlanSessionState,
) -> Result<(), anyhow::Error> {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    meta.plan_mode = plan.enabled().then_some(true);
    meta.plan_file = plan.plan_file();
    pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await
}

async fn load_plan_review_pending(sessions_dir: &Path, session_path: &Path) -> bool {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => meta.plan_review_pending.unwrap_or(false),
        Err(_) => false,
    }
}

async fn write_plan_review_pending_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    pending: bool,
) -> Result<(), anyhow::Error> {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    meta.plan_review_pending = pending.then_some(true);
    pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await
}

/// Re-sync plan mode after a session switch: the flag follows the opened
/// session's sidecar. Emits `PlanModeChanged` so the facade chip tracks the
/// session it now mirrors; instructions re-render when the target session
/// plans.
async fn resync_plan_state(
    sessions_dir: &Path,
    session_path: &Path,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let (enabled, plan_file) = load_plan_state(sessions_dir, session_path).await;
    plan.set(enabled, plan_file);
    plan.set_active_instructions(enabled.then(render_plan_instructions).flatten());
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::PlanModeChanged { enabled },
    )));
}

async fn write_approval_mode_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    mode: ApprovalMode,
) -> Result<(), anyhow::Error> {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    let raw = serde_json::to_value(mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "autopilot".to_string());
    meta.approval_mode = Some(raw);
    pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await
}

/// Re-read the session's persisted approval mode after a session switch and
/// align the gate + the facade's chip with it.
async fn resync_approval_mode(
    session: &AgentSession,
    sessions_dir: &Path,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let mode = load_approval_mode(sessions_dir, session.path()).await;
    state.gate.set_mode(mode);
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::ApprovalModeChanged { mode },
    )));
}

/// Mirror the session's authoritative transcript into engine state, then
/// re-attach the registry slash turns' compact display forms from the
/// sidecar (the transcript stores only the expanded macro/skill body, so
/// the attach is what keeps a reloaded thread's bubbles compact).
async fn sync_history(session: &AgentSession, sessions_dir: &Path, state: &Arc<EngineState>) {
    let mut mapped = adapt::harness_messages_to_messages(session.harness_messages());
    attach_registry_displays(
        &mut mapped,
        &load_registry_displays(sessions_dir, session.path()).await,
    );
    *state.history.lock().unwrap() = mapped;
}

/// The compact display forms persisted per user-message ordinal by
/// `Thread::persist_registry_display`. Missing sidecar or field reads as
/// empty (no registry turns yet).
async fn load_registry_displays(
    sessions_dir: &Path,
    session_path: &Path,
) -> std::collections::HashMap<usize, String> {
    pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .map(|meta| meta.registry_displays)
        .unwrap_or_default()
}

/// Drop the persisted registry display forms. A compaction rebuilds the
/// transcript as a summary user message plus the retained tail, which shifts
/// every persisted display ordinal; the stale forms would otherwise attach to
/// the wrong user prompt on the next `sync_history`. New registry turns after
/// the compaction persist fresh ordinals over the rebuilt sequence.
async fn clear_registry_displays(sessions_dir: &Path, session_path: &Path) {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    if meta.registry_displays.is_empty() {
        return;
    }
    meta.registry_displays.clear();
    if let Err(err) = pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await {
        tracing::warn!(error = %err, "failed to clear registry display text");
    }
}

/// `clear_registry_displays` from a harness event listener, which is
/// synchronous — the clear runs on the runtime and its outcome is only a
/// display form, so a lost write just narrows the reload window.
fn clear_registry_displays_spawn(sessions_dir: PathBuf, session_path: PathBuf) {
    tokio::spawn(async move {
        clear_registry_displays(&sessions_dir, &session_path).await;
    });
}

/// Re-attach `display_text` to the user prompt message at each persisted
/// ordinal. The ordinal counts `Role::User` messages with a `User`
/// provenance — the same set `Thread` counts when persisting — so steers
/// (user prompts too) and tool results (excluded) align between the two.
fn attach_registry_displays(
    history: &mut [Message],
    displays: &std::collections::HashMap<usize, String>,
) {
    if displays.is_empty() {
        return;
    }
    let mut ordinal = 0usize;
    for message in history {
        if message.role == crate::language_model::Role::User
            && message.provenance == crate::message::MessageProvenance::User
        {
            if let Some(text) = displays.get(&ordinal) {
                message.ui.get_or_insert_with(Default::default).display_text = Some(text.clone());
            }
            ordinal += 1;
        }
    }
}

/// Persist the bound project in the session sidecar so the sidebar groups
/// the session under its project folder across restarts.
async fn write_project_sidecar(sessions_dir: &Path, session_path: &Path, project: &Path) {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    meta.project = Some(project.to_string_lossy().to_string());
    if let Err(err) = pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await {
        tracing::warn!(error = %err, "failed to persist session project");
    }
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
    let mut per_model_cost = HashMap::new();
    for entry in &stats.per_model {
        per_model.insert(entry.key.clone(), token_usage_from_totals(&entry.totals));
        if entry.totals.cost > 0.0 {
            per_model_cost.insert(entry.key.clone(), entry.totals.cost);
        }
    }
    *state.cumulative.lock().unwrap() = cumulative;
    *state.per_model.lock().unwrap() = per_model;
    *state.cumulative_cost.lock().unwrap() = stats.tokens.cost;
    *state.per_model_cost.lock().unwrap() = per_model_cost;
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
    *state.cumulative_cost.lock().unwrap() = 0.0;
    *state.per_model_cost.lock().unwrap() = HashMap::new();
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
    /// message boundaries, block start/end markers) map to nothing.
    /// `ToolCallAuthorization` never comes from this mapping — the approval
    /// gate (`pi_approval`) emits it directly while parked on a verdict.
    /// `Plan*` and sub-agent events remain manox-only and are never produced.
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
            } => {
                let mut events = vec![ThreadEvent::ToolCall {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    title: tool_title(tool_name, arguments),
                    status: ToolCallStatus::Running,
                    input: Some(arguments.clone()),
                }];
                // A spawned sub-agent also lands as a rail observation row
                // (the conversation shows the Agent tool call card; the rail
                // tracks the nested session's lifecycle).
                if tool_name == crate::tools::AGENT {
                    events.push(ThreadEvent::SubagentProgress {
                        id: tool_call_id.clone(),
                        subagent_type: arguments
                            .get("subagent_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tool_uses: 0,
                        token_usage: crate::language_model::TokenUsage::default(),
                        latest_activity: arguments.get("prompt").and_then(|v| v.as_str()).map(
                            |prompt| {
                                let flat: String =
                                    prompt.split_whitespace().collect::<Vec<_>>().join(" ");
                                let mut chars = flat.chars();
                                let head: String = chars.by_ref().take(60).collect();
                                if chars.next().is_some() {
                                    format!("{head}…")
                                } else {
                                    head
                                }
                            },
                        ),
                        status: ToolCallStatus::Running,
                    });
                }
                events
            }
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
            } => {
                let status = if *is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Success
                };
                let mut events = vec![
                    ThreadEvent::ToolCall {
                        id: tool_call_id.clone(),
                        name: tool_name.clone(),
                        title: tool_name.clone(),
                        status,
                        input: None,
                    },
                    ThreadEvent::ToolResult {
                        id: tool_call_id.clone(),
                        output: tool_result_text(result),
                        is_error: *is_error,
                    },
                ];
                // Close the sub-agent's rail observation row (the row itself
                // was created by the start event; empty type here is fine —
                // the upsert keeps the existing entry's fields).
                if tool_name == crate::tools::AGENT {
                    events.push(ThreadEvent::SubagentProgress {
                        id: tool_call_id.clone(),
                        subagent_type: String::new(),
                        tool_uses: 0,
                        token_usage: crate::language_model::TokenUsage::default(),
                        latest_activity: None,
                        status,
                    });
                }
                events
            }
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
                    // Plan blocks surface through the PlanReady review flow;
                    // strip them from the displayed transcript (the session
                    // jsonl keeps the raw text, manox parity).
                    for block in blocks.iter_mut() {
                        if let MessageContent::Text(text) = block {
                            *text = crate::proposed_plan::strip_proposed_plan_blocks(text);
                        }
                    }
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
            "Read" | "Write" | "Ls" => match arg("path") {
                Some(path) => format!("{name} {path}"),
                None => name.to_string(),
            },
            "Edit" | "EditDiff" => match arg("path") {
                Some(path) => format!("Edit {path}"),
                None => "Edit".to_string(),
            },
            "Grep" => match (arg("pattern"), arg("path")) {
                (Some(pattern), Some(path)) => format!("Grep {pattern} {path}"),
                (Some(pattern), None) => format!("Grep {pattern}"),
                _ => "Grep".to_string(),
            },
            "Find" => match arg("pattern") {
                Some(pattern) => format!("Find {pattern}"),
                None => "Find".to_string(),
            },
            "Bash" => match arg("command") {
                Some(command) => format!("$ {command}"),
                None => "Bash".to_string(),
            },
            "BashOutput" => "BashOutput".to_string(),
            "TaskStop" => "TaskStop".to_string(),
            "Agent" => match arg("subagent_type") {
                Some(kind) => format!("Agent {kind}"),
                None => "Agent".to_string(),
            },
            _ => name.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_mode_sidecar_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-1.jsonl");

        // Fresh session: no sidecar -> default.
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            ApprovalMode::AutoPilot
        );

        write_approval_mode_sidecar(dir.path(), &session, ApprovalMode::Danger)
            .await
            .unwrap();
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            ApprovalMode::Danger
        );

        write_approval_mode_sidecar(dir.path(), &session, ApprovalMode::AutoPilot)
            .await
            .unwrap();
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            ApprovalMode::AutoPilot
        );
    }

    #[tokio::test]
    async fn attach_registry_displays_restores_sidecar_compact_forms() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-display.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            registry_displays: [
                (0usize, "/gitwork:deliver fast".to_string()),
                (2usize, "/healthz".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();

        let displays = load_registry_displays(dir.path(), &session).await;
        // Ordinals count user prompts only: a tool result (user role, tool
        // provenance) and assistant turns must not consume one.
        let mut history = vec![
            Message::user("expanded macro body".to_string()), // ordinal 0
            Message::user("plain turn".to_string()),          // ordinal 1
            Message::user_with_content(vec![MessageContent::ToolResult(
                crate::language_model::LanguageModelToolResult {
                    tool_use_id: "tu_1".into(),
                    tool_name: "Read".into(),
                    is_error: false,
                    content: "ok".into(),
                },
            )]),
            Message::assistant(vec![MessageContent::Text("reply".into())]),
            Message::user("expanded skill body".to_string()), // ordinal 2
        ];
        attach_registry_displays(&mut history, &displays);

        assert_eq!(
            history[0]
                .ui
                .as_ref()
                .and_then(|ui| ui.display_text.as_deref()),
            Some("/gitwork:deliver fast")
        );
        assert!(history[1].ui.is_none(), "plain turn keeps no display text");
        assert!(
            history[2].ui.is_none(),
            "tool result never consumes a display ordinal"
        );
        assert!(
            history[3].ui.is_none(),
            "assistant turns never get display text"
        );
        assert_eq!(
            history[4]
                .ui
                .as_ref()
                .and_then(|ui| ui.display_text.as_deref()),
            Some("/healthz")
        );
    }

    #[tokio::test]
    async fn clear_registry_displays_drops_sidecar_ordinals() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-display-clear.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            registry_displays: [(0usize, "/gitwork:deliver fast".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();

        clear_registry_displays(dir.path(), &session).await;

        let displays = load_registry_displays(dir.path(), &session).await;
        assert!(
            displays.is_empty(),
            "a compaction clears the stale display ordinals"
        );
    }

    #[tokio::test]
    async fn clear_registry_displays_is_noop_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-display-empty.jsonl");

        clear_registry_displays(dir.path(), &session).await;

        let displays = load_registry_displays(dir.path(), &session).await;
        assert!(displays.is_empty());
    }

    #[tokio::test]
    async fn approval_mode_sidecar_tolerates_unknown_values() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-2.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            approval_mode: Some("yolo".to_string()),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            ApprovalMode::AutoPilot,
            "unknown persisted modes fall back to the default"
        );
    }

    #[tokio::test]
    async fn approval_mode_write_preserves_other_sidecar_fields() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-3.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            title: Some("my thread".to_string()),
            project: Some("/tmp/proj".to_string()),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();

        write_approval_mode_sidecar(dir.path(), &session, ApprovalMode::Danger)
            .await
            .unwrap();

        let loaded = pi_extensions::session_meta::load(dir.path(), &session)
            .await
            .unwrap();
        assert_eq!(loaded.title.as_deref(), Some("my thread"));
        assert_eq!(loaded.project.as_deref(), Some("/tmp/proj"));
        assert_eq!(loaded.approval_mode.as_deref(), Some("danger"));
    }

    #[test]
    fn steer_message_carries_images_behind_text() {
        let msg = steer_message(
            "look at this".to_string(),
            vec![pi::types::ContentBlock::Image {
                data: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
            }],
        );
        let pi::types::AgentMessage::User { content, .. } = &msg else {
            panic!("steer message must be a user message");
        };
        assert_eq!(content.len(), 2, "text first, then the image block");
        assert!(matches!(
            &content[0],
            pi::types::ContentBlock::Text { text, .. } if text == "look at this"
        ));
        assert!(matches!(
            &content[1],
            pi::types::ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
        ));
    }

    #[test]
    fn agent_tool_start_maps_to_subagent_progress_row() {
        let events =
            adapt::agent_event_to_thread_events(&pi::types::AgentEvent::ToolExecutionStart {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                arguments: serde_json::json!({
                    "subagent_type": "Explore",
                    "prompt": "find the auth module and summarize its structure",
                }),
            });
        assert_eq!(events.len(), 2, "tool card + rail observation row");
        match &events[1] {
            crate::thread::ThreadEvent::SubagentProgress {
                id,
                subagent_type,
                latest_activity,
                status,
                ..
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(subagent_type, "Explore");
                assert_eq!(
                    latest_activity.as_deref(),
                    Some("find the auth module and summarize its structure")
                );
                assert_eq!(*status, crate::thread::ToolCallStatus::Running);
            }
            other => panic!("expected SubagentProgress, got {other:?}"),
        }
    }

    #[test]
    fn agent_tool_end_closes_subagent_progress_row() {
        let events =
            adapt::agent_event_to_thread_events(&pi::types::AgentEvent::ToolExecutionEnd {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                result: pi::tool::AgentToolResult::text("done"),
                is_error: false,
            });
        assert_eq!(events.len(), 3, "tool card + result + rail row");
        match &events[2] {
            crate::thread::ThreadEvent::SubagentProgress { id, status, .. } => {
                assert_eq!(id, "call-1");
                assert_eq!(*status, crate::thread::ToolCallStatus::Success);
            }
            other => panic!("expected SubagentProgress, got {other:?}"),
        }
    }

    #[test]
    fn non_agent_tools_emit_no_subagent_progress() {
        let events =
            adapt::agent_event_to_thread_events(&pi::types::AgentEvent::ToolExecutionStart {
                tool_call_id: "call-2".into(),
                tool_name: "Read".into(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            });
        assert_eq!(events.len(), 1, "plain tools keep a single tool card");
    }

    #[test]
    fn adapt_strips_proposed_plan_blocks_from_assistant_text() {
        let plan = "## Steps\n- do the thing";
        let messages = vec![pi::types::AgentMessage::Assistant {
            content: vec![pi::types::ContentBlock::Text {
                text: format!(
                    "Here is my plan.\n\n<proposed_plan>\n{plan}\n</proposed_plan>\n\nShall we?"
                ),
                signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(pi::types::StopReason::Stop),
            raw_stop_reason: None,
            usage: Box::new(pi::types::Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }];
        let mapped = adapt::harness_messages_to_messages(&messages);
        assert_eq!(mapped.len(), 1);
        let text = mapped[0]
            .content
            .iter()
            .find_map(|c| match c {
                crate::language_model::MessageContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(
            !text.contains("<proposed_plan>"),
            "plan block must not render"
        );
        assert!(text.contains("Here is my plan."));
        assert!(text.contains("Shall we?"));
    }
}
