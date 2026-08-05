//! `PiSession` — the pi harness session presented as a workspace thread.
//!
//! A gpui entity that owns a tokio actor around a pi `AgentSession`. The
//! actor drives the pi run loop; run events flow back through a channel, are
//! adapted into `ThreadEvent`s (see [`super::adapt`]), and emitted on this
//! entity so the workspace's existing `subscribe_thread` handler renders them
//! unchanged. History is exposed as `agent::Message`s so the rebuild path
//! (`ConversationState::rebuild_from_messages`) is reused as-is.
//!
//! The entity duck-types the subset of `agent::Thread`'s API the workspace
//! calls, so call sites compile under both harness features. manox-only
//! affordances (pin/archive/notes/approval mode/model switch) are inert
//! no-ops here; capabilities that need real wiring carry `TODO(pi-wire)`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent::ReasoningEffort;
use agent::background_task::TaskSnapshot;
use agent::db::UiNoteRecord;
use agent::goal::ThreadGoal;
use agent::language_model::{AnyLanguageModel, MessageContent, TokenUsage};
use agent::team::Team;
use agent::thread::{ApprovalMode, WorktreeState};
use agent::{Message, MessageUiMetadata, ThreadEvent, ThreadId};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter};
use pi::coding_agent::{ModelRuntime, create_agent_session};
use pi::ext_point_agent::AgentRegistry;
use pi::tool::AgentTool as PiAgentTool;
use pi::types::{AgentEvent, AgentMessage, ContentBlock, Model as PiModel};
use pi_extensions::agents::{SubagentTool, register_defaults};
use pi_extensions::bash::BashTool;
use pi_extensions::bash::orchestration::BackgroundManager;
use pi_extensions::bash::persistent::PersistentShellOperations;
use pi_extensions::{BackgroundRegistry, BashOutputTool, TaskStopTool};
use tokio::sync::mpsc;

use crate::pi_backend::adapt;

/// Commands the gpui side sends to the pi actor.
enum SessionCmd {
    /// Start a turn with the given user text (already appended to history).
    Prompt(String),
    /// Inject a steer into the running turn; the id confirms the bubble.
    Steer { id: String, text: String },
    /// Abort the running turn.
    Abort,
    /// Re-point the session at a new project dir (only before first use).
    Reconfigure { cwd: PathBuf },
    /// Close the session and stop the actor.
    Shutdown,
}

/// Notices the pi actor sends back to the gpui side.
enum BackendNotice {
    /// A pi run event to adapt into `ThreadEvent`s. Boxed: `AgentEvent` is
    /// ~300B and would otherwise dominate the enum's niche layout.
    Event(Box<AgentEvent>),
    /// The session finished building/restoring.
    Ready { restored: bool },
    /// Authoritative history (on restore, and after every settled run).
    History(Vec<AgentMessage>),
    /// The turn loop unwound and released the running slot.
    Settled {
        cancelled: bool,
        failed: bool,
        /// Steers confirmed injected during the run.
        steered: Vec<String>,
        /// Steers stranded by an aborted/failed run.
        stranded: Vec<String>,
    },
    /// The session could not be built at all.
    Fatal(anyhow::Error),
}

/// The workspace's thread for harness-pi builds.
pub struct PiSession {
    pub id: ThreadId,
    cwd: PathBuf,
    project: Option<PathBuf>,
    model: Option<AnyLanguageModel>,
    messages: Vec<Message>,
    usage: HashMap<String, TokenUsage>,
    ui_notes: Vec<UiNoteRecord>,
    approval_mode: ApprovalMode,
    reasoning_effort: ReasoningEffort,
    running: bool,
    pinned: bool,
    archived: bool,
    restored: bool,
    /// Text of user messages inserted since the last run, drained by
    /// `run_turn` into one prompt (mirrors `Thread`'s request build).
    pending_prompts: Vec<String>,
    /// UI metadata of the most recently inserted user turn, re-attached to
    /// the authoritative history's last user message after each refresh so
    /// rebuilt bubbles keep their model/time row.
    last_user_ui: Option<MessageUiMetadata>,
    /// Steer message ids handed to the actor this run, awaiting settlement.
    run_steers: VecDeque<String>,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
}

impl EventEmitter<ThreadEvent> for PiSession {}

impl PiSession {
    /// Matches `Thread::new`'s signature so `Workspace::new` swaps backends
    /// at one call site. Spawns the pi actor (build-or-restore) and the gpui
    /// drainer that turns backend notices into `ThreadEvent`s.
    pub fn new(id: ThreadId, cwd: PathBuf, cx: &mut App) -> Entity<Self> {
        let model = agent::provider::registry::global()
            .models()
            .first()
            .cloned();
        cx.new(|cx| {
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let (notice_tx, mut notice_rx) = mpsc::unbounded_channel();

            // The pi actor: owns the `AgentSession` on the tokio runtime.
            let actor_cwd = cwd.clone();
            let actor_model = model.clone();
            agent::runtime::handle().spawn(async move {
                run_actor(actor_cwd, actor_model, cmd_rx, notice_tx).await;
            });

            // The gpui drainer: adapts backend notices onto this entity.
            cx.spawn(async move |this, cx| {
                while let Some(notice) = notice_rx.recv().await {
                    let mut rebuild_restore = false;
                    let ok = this
                        .update(cx, |session: &mut PiSession, cx| match notice {
                            BackendNotice::Event(event) => {
                                for te in adapt::agent_event_to_thread_events(&event) {
                                    cx.emit(te);
                                }
                            }
                            BackendNotice::Ready { restored } => {
                                session.restored = restored;
                            }
                            BackendNotice::History(history) => {
                                let mut mapped = adapt::harness_messages_to_messages(&history);
                                if let Some(ui) = session.last_user_ui.clone()
                                    && let Some(last_user) = mapped.iter_mut().rev().find(|m| {
                                        matches!(m.role, agent::language_model::Role::User)
                                    })
                                {
                                    last_user.ui = Some(ui);
                                }
                                let restored_now = session.restored && session.messages.is_empty();
                                session.messages = mapped;
                                if restored_now && !session.messages.is_empty() {
                                    rebuild_restore = true;
                                }
                                cx.notify();
                            }
                            BackendNotice::Settled {
                                cancelled,
                                failed,
                                steered,
                                stranded,
                            } => {
                                for message_id in steered {
                                    cx.emit(ThreadEvent::SteerInjected { message_id });
                                }
                                session.running = false;
                                session.run_steers.clear();
                                cx.emit(ThreadEvent::TurnFinished {
                                    cancelled,
                                    failed,
                                    stranded_steer_ids: stranded,
                                });
                            }
                            BackendNotice::Fatal(err) => {
                                session.running = false;
                                cx.emit(ThreadEvent::Error(err));
                            }
                        })
                        .is_ok();
                    if !ok {
                        break;
                    }
                    if rebuild_restore && let Some(workspace) = crate::dispatch::workspace_global()
                    {
                        workspace.update(cx, |workspace, cx| {
                            workspace.rebuild_conversation_from_thread(cx);
                        });
                    }
                }
            })
            .detach();

            Self {
                id,
                cwd,
                project: None,
                model,
                messages: Vec::new(),
                usage: HashMap::new(),
                ui_notes: Vec::new(),
                approval_mode: ApprovalMode::default(),
                reasoning_effort: ReasoningEffort::default(),
                running: false,
                pinned: false,
                archived: false,
                restored: false,
                pending_prompts: Vec::new(),
                last_user_ui: None,
                run_steers: VecDeque::new(),
                cmd_tx,
            }
        })
    }

    // ── Thread duck-type: the turn pipeline ────────────────────────────────

    pub fn insert_user_message_with_ui_metadata(
        &mut self,
        text: String,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        let mut message = Message::user(text.clone());
        message.ui = ui.clone();
        self.messages.push(message);
        self.pending_prompts.push(text);
        self.last_user_ui = ui;
        cx.notify();
    }

    pub fn insert_user_message_with_content_and_ui_metadata(
        &mut self,
        content: Vec<MessageContent>,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        // TODO(pi-wire): image attachments — pi prompts are text-only in this
        // stage; image blocks are dropped from the prompt text.
        let text: String = content
            .iter()
            .filter_map(|c| match c {
                MessageContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut message = Message::user_with_content(content);
        message.ui = ui.clone();
        self.messages.push(message);
        if !text.trim().is_empty() {
            self.pending_prompts.push(text);
        }
        self.last_user_ui = ui;
        cx.notify();
    }

    pub fn enqueue_steer(
        &mut self,
        content: Vec<MessageContent>,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) -> String {
        let text: String = content
            .iter()
            .filter_map(|c| match c {
                MessageContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut message = Message::user_with_content(content);
        message.ui = ui;
        let id = message.id.clone();
        self.run_steers.push_back(id.clone());
        let _ = self.cmd_tx.send(SessionCmd::Steer {
            id: id.clone(),
            text,
        });
        cx.notify();
        // The canonical message joins history at the next refresh (pi owns the
        // transcript); the workspace renders the optimistic bubble until
        // `SteerInjected` confirms.
        id
    }

    pub fn run_turn(&mut self, cx: &mut Context<Self>) {
        if self.running || self.pending_prompts.is_empty() {
            return;
        }
        let prompt = std::mem::take(&mut self.pending_prompts).join("\n\n");
        self.running = true;
        cx.emit(ThreadEvent::TurnStarted);
        let _ = self.cmd_tx.send(SessionCmd::Prompt(prompt));
        cx.notify();
    }

    pub fn cancel(&mut self, _cx: &mut Context<Self>) {
        let _ = self.cmd_tx.send(SessionCmd::Abort);
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    // ── Thread duck-type: read accessors ───────────────────────────────────

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn project(&self) -> Option<&PathBuf> {
        self.project.as_ref()
    }

    pub fn model(&self) -> Option<&AnyLanguageModel> {
        self.model.as_ref()
    }

    pub fn display_title(&self) -> String {
        // Mechanical summary like `Thread`'s fallback: the first user prompt,
        // trimmed to a title-sized prefix.
        self.messages
            .iter()
            .find(|m| matches!(m.role, agent::language_model::Role::User))
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    MessageContent::Text(t) if !t.trim().is_empty() => Some(t.trim()),
                    _ => None,
                })
            })
            .map(|t| {
                let flat: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let mut chars = flat.chars();
                let head: String = chars.by_ref().take(60).collect();
                if chars.next().is_some() {
                    format!("{head}…")
                } else {
                    head
                }
            })
            .unwrap_or_else(|| "Manox Pi".to_string())
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.reasoning_effort
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    pub fn archived(&self) -> bool {
        self.archived
    }

    pub fn last_user_message_id(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, agent::language_model::Role::User))
            .map(|m| m.id.as_str())
    }

    pub fn has_interacted(&self) -> bool {
        self.messages
            .iter()
            .any(|m| matches!(m.role, agent::language_model::Role::User))
    }

    pub fn request_token_usage(&self) -> &HashMap<String, TokenUsage> {
        &self.usage
    }

    pub fn last_request_token_usage(&self) -> Option<TokenUsage> {
        None
    }

    pub fn ui_notes(&self) -> &[UiNoteRecord] {
        &self.ui_notes
    }

    pub fn worktree(&self) -> Option<&WorktreeState> {
        None
    }

    pub fn team(&self) -> Option<&Entity<Team>> {
        None
    }

    pub fn goal(&self) -> Option<&ThreadGoal> {
        None
    }

    pub fn background_task_snapshots(&self) -> Vec<TaskSnapshot> {
        // TODO(pi-wire): surface pi-extensions background tasks in the cockpit.
        Vec::new()
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for m in &self.messages {
            let heading = match m.role {
                agent::language_model::Role::User => "## User",
                agent::language_model::Role::Assistant => "## Assistant",
                agent::language_model::Role::System => "## System",
            };
            out.push_str(heading);
            out.push_str("\n\n");
            for c in &m.content {
                match c {
                    MessageContent::Text(t) | MessageContent::Thinking { text: t, .. } => {
                        out.push_str(t);
                        out.push('\n');
                    }
                    MessageContent::Image { .. } => out.push_str("(image)\n"),
                    MessageContent::ToolUse(u) => {
                        out.push_str(&format!("```tool_use {}\n{}\n```\n", u.name, u.raw_input));
                    }
                    MessageContent::ToolResult(r) => {
                        out.push_str(&format!("```tool_result\n{}\n```\n", r.content));
                    }
                    MessageContent::Compaction(s) => {
                        out.push_str(&format!("```compaction\n{s}\n```\n"));
                    }
                }
            }
            out.push('\n');
        }
        out
    }

    // ── Thread duck-type: setters (inert where manox-only) ─────────────────

    pub fn set_project(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.has_interacted() {
            return;
        }
        self.cwd = dir.clone();
        self.project = Some(dir.clone());
        let _ = self.cmd_tx.send(SessionCmd::Reconfigure { cwd: dir });
        cx.notify();
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        // TODO(pi-wire): approval gating — the pi toolset runs ungated in this
        // stage; the mode is recorded for the chip but gates nothing.
        self.approval_mode = mode;
        cx.notify();
    }

    pub fn set_model(&mut self, model: AnyLanguageModel, cx: &mut Context<Self>) {
        // TODO(pi-wire): model switching — the actor builds the session with
        // the registry's first model; hot-swapping needs a session rebuild.
        self.model = Some(model);
        cx.notify();
    }

    pub fn set_reasoning_effort(&mut self, effort: ReasoningEffort, cx: &mut Context<Self>) {
        // TODO(pi-wire): map onto pi's thinking level.
        self.reasoning_effort = effort;
        cx.notify();
    }

    pub fn set_pinned(&mut self, pinned: bool, cx: &mut Context<Self>) {
        self.pinned = pinned;
        cx.notify();
    }

    pub fn set_archived(&mut self, archived: bool, cx: &mut Context<Self>) {
        self.archived = archived;
        cx.notify();
    }

    pub fn pending_steer_ids(&self) -> Vec<String> {
        self.run_steers.iter().cloned().collect()
    }

    pub fn cancel_pending_steer(&mut self, id: &str) -> bool {
        if let Some(pos) = self.run_steers.iter().position(|s| s == id) {
            self.run_steers.remove(pos);
            // TODO(pi-wire): pi's steering queue cannot dequeue; the text may
            // still inject. Removing the UI card is the stage-1 semantic.
            true
        } else {
            false
        }
    }

    pub fn push_ui_note(&mut self, note: UiNoteRecord) {
        self.ui_notes.push(note);
    }

    pub fn submit_command(&mut self, _name: &str, _args: &str, _cx: &mut Context<Self>) -> bool {
        // TODO(pi-wire): slash commands are a manox registry feature.
        false
    }

    pub fn submit_skill(&mut self, _key: &str, _args: &str, _cx: &mut Context<Self>) -> bool {
        // TODO(pi-wire): skills are a manox registry feature.
        false
    }
}

impl Drop for PiSession {
    fn drop(&mut self) {
        // The actor owns the session; ask it to close gracefully. If the
        // channel is already gone the actor exited on its own.
        let _ = self.cmd_tx.send(SessionCmd::Shutdown);
    }
}

// ── The pi actor ───────────────────────────────────────────────────────────

/// Fixed single-session dir under the manox config dir. Session list UI comes
/// in a later stage; for now the newest session is restored at startup.
fn pi_session_dir() -> PathBuf {
    agent::paths::manox_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("pi-sessions")
}

/// Minimal builtin coding-agent prompt. Deliberately NOT the manox
/// `system_prompt` assembly — that belongs to the frozen harness.
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
fn build_tools(cwd: &Path, runtime: &ModelRuntime, model: &PiModel) -> Vec<Arc<dyn PiAgentTool>> {
    let background = Arc::new(BackgroundRegistry::new());
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let bash = BashTool::new(
        Arc::new(PersistentShellOperations::new(cwd)),
        background.clone(),
    )
    .with_manager(Arc::clone(&manager));

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

    vec![
        Arc::new(pi::tools::read::ReadTool),
        Arc::new(pi::tools::write::WriteTool),
        Arc::new(pi::tools::edit::EditTool),
        Arc::new(pi::tools::grep::GrepTool),
        Arc::new(pi::tools::find::FindTool),
        Arc::new(pi::tools::ls::LsTool),
        Arc::new(bash),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(TaskStopTool::new(background)),
        Arc::new(subagent),
    ]
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

/**/
fn subscribe_session(
    session: &pi::coding_agent::AgentSession,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) -> pi::agent::Subscription {
    let event_tx = notice_tx.clone();
    session.subscribe(Arc::new(move |event, _cancel| {
        let tx = event_tx.clone();
        Box::pin(async move {
            let _ = tx.send(BackendNotice::Event(Box::new(event)));
        })
    }))
}

async fn run_actor(
    cwd: PathBuf,
    model: Option<AnyLanguageModel>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
) {
    let Some(manox_model) = model else {
        let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
            "no model configured — add a provider in Settings"
        )));
        return;
    };
    let pi_model = match agent::pi_bridge::map_model(&manox_model) {
        Ok(m) => m,
        Err(e) => {
            let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(e)));
            return;
        }
    };
    let resolver = match agent::pi_bridge::stream_resolver(&manox_model) {
        Ok(r) => r,
        Err(e) => {
            let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(e)));
            return;
        }
    };
    let runtime = ModelRuntime::new(resolver);
    let session_dir = pi_session_dir();

    // Restore the newest session, else start fresh. Tool cwd follows the
    // restored session's project dir (the builder's `open` re-pins cwd too).
    let repo = pi::session::repository::SessionRepository::new(&session_dir);
    // Newest first; skip sessions that never saw a message (an empty session
    // file would re-pin the launch cwd to a stale project choice).
    let latest = repo
        .list()
        .await
        .ok()
        .and_then(|list| list.into_iter().find(|info| info.message_count > 0));
    let mut restored = false;
    let mut session = None;
    if let Some(info) = latest {
        let tool_cwd = PathBuf::from(info.cwd.clone());
        let builder = create_agent_session()
            .with_cwd(tool_cwd.clone())
            .with_session_dir(session_dir.clone())
            .with_model_runtime(runtime.clone())
            .with_model(pi_model.clone())
            .with_system_prompt(system_prompt(&tool_cwd))
            .with_tools(build_tools(&tool_cwd, &runtime, &pi_model));
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
            let builder = create_agent_session()
                .with_cwd(cwd.clone())
                .with_session_dir(session_dir.clone())
                .with_model_runtime(runtime.clone())
                .with_model(pi_model.clone())
                .with_system_prompt(system_prompt(&cwd))
                .with_tools(build_tools(&cwd, &runtime, &pi_model));
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

    // Stream run events back to the gpui drainer. Re-registered after a
    // `Reconfigure` rebuilds the session (listeners live on the old Agent).
    let mut _subscription = subscribe_session(&session, &notice_tx);

    let _ = notice_tx.send(BackendNotice::Ready { restored });
    if restored {
        let _ = notice_tx.send(BackendNotice::History(session.harness_messages().to_vec()));
    }

    let mut run_steers: Vec<String> = Vec::new();
    let mut shutdown_after_run = false;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCmd::Prompt(text) => {
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
                                Some(SessionCmd::Shutdown) => shutdown_after_run = true,
                                Some(_) => {} // queued prompts/reconfigs wait for settle
                                None => {
                                    // Entity dropped mid-run: abort, settle, exit.
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
                    let _ =
                        notice_tx.send(BackendNotice::Event(Box::new(AgentEvent::MessageEnd {
                            message: Box::new(AgentMessage::Assistant {
                                content: Vec::new(),
                                model: String::new(),
                                provider: String::new(),
                                api: String::new(),
                                response_model: None,
                                response_id: None,
                                diagnostics: None,
                                stop_reason: Some(pi::types::StopReason::Error),
                                raw_stop_reason: None,
                                usage: Box::default(),
                                error_message: Some(format!("{err:#}")),
                                timestamp: chrono::Utc::now(),
                            }),
                        })));
                }
                let _ = notice_tx.send(BackendNotice::History(session.harness_messages().to_vec()));
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
            SessionCmd::Abort => {
                session.abort();
            }
            SessionCmd::Reconfigure { cwd } => {
                // Valid only before the first interaction (workspace gates
                // this); rebuild the session against the new project dir.
                let _ = session.close().await;
                let builder = create_agent_session()
                    .with_cwd(cwd.clone())
                    .with_session_dir(session_dir.clone())
                    .with_model_runtime(runtime.clone())
                    .with_model(pi_model.clone())
                    .with_system_prompt(system_prompt(&cwd))
                    .with_tools(build_tools(&cwd, &runtime, &pi_model));
                match builder.build().await {
                    Ok(s) => {
                        session = s;
                        _subscription = subscribe_session(&session, &notice_tx);
                    }
                    Err(err) => {
                        let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                            "pi session rebuild failed: {err}"
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
