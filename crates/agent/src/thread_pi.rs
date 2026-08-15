// The pi-backed `Thread` facade (built with `feature = "harness-pi"`).
//
// A gpui entity that owns a tokio actor around a pi `AgentSession` through
// the `ThreadEngine` contract. Run events flow back through a channel, are
// adapted into `ThreadEvent`s (see `pi_engine::adapt`), and are emitted on
// this entity so the workspace's existing `subscribe_thread` handler renders
// them unchanged. History is exposed as `agent::Message`s so the rebuild
// path (`ConversationState::rebuild_from_messages`) is reused as-is.
//
// The public surface mirrors the manox `Thread`'s — the workspace compiles
// against one shape. manox-only affordances (pin/archive/notes/goal/team/
// worktree) are inert here; capabilities that need real wiring carry a stub
// with the reason. Approval mode is live: the facade records it, the
// engine's gate enforces it, and the sidecar persists it.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter};
use serde::{Deserialize, Serialize};

use crate::background_task::TaskSnapshot;
use crate::db::UiNoteRecord;
use crate::goal::ThreadGoal;
use crate::goal_tools::GoalBridge;
use crate::language::Language;
use crate::language_model::{MessageContent, ReasoningEffort, Role, StopReason, TokenUsage};
use pi::types::Model as PiModel;
use crate::message::{Message, MessageUiMetadata};
use crate::thread_engine::{BackendNotice, SpawnedEngine, ThreadEngine};
use crate::team::Team;

/// Stable `Thread` id used for persistence.
#[derive(Debug, Clone)]
pub struct ThreadId(pub String);

/// Tool call status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCallStatus {
    PendingApproval,
    Running,
    Success,
    Continued,
    Error,
    Denied,
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SideCallMetric {
    pub purpose: String,
    pub model: String,
    pub calls: u64,
    pub token_usage: TokenUsage,
    pub latency_ms: u64,
}

/// User-facing approval policy. `AutoPilot` routes approval-required tool
/// calls through the safety reviewer with user escalation; `Danger` runs
/// everything without prompting. Enforced by the engine's approval gate
/// (`pi_approval`); persisted in the session sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    #[serde(rename = "autopilot")]
    #[default]
    AutoPilot,
    #[serde(rename = "danger")]
    Danger,
}

impl ApprovalMode {
    pub fn from_i64(v: i64) -> Self {
        match v {
            1 | 2 => Self::Danger,
            _ => Self::AutoPilot,
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Self::AutoPilot => 0,
            Self::Danger => 1,
        }
    }
}

/// Session-scoped state for a thread inside a git worktree. The pi backend
/// never enters worktrees in this stage, so the type exists only to keep the
/// facade surface identical to the manox build.
pub struct WorktreeState {
    pub path: PathBuf,
    pub prior_cwd: PathBuf,
    pub branch: String,
    pub git_common_dir: PathBuf,
    pub subagent_created: bool,
}

/// History-loading state of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryPhase {
    /// No history pending (fresh / landing threads); the message list is
    /// final as soon as it exists.
    Ready,
    /// An existing session is being restored. Display-only preview batches
    /// may stream into `messages` while the authoritative restore runs; the
    /// workspace hides the composer and shows a loading indicator.
    Loading,
}

impl HistoryPhase {
    pub fn is_loading(self) -> bool {
        matches!(self, Self::Loading)
    }
}

/// Events emitted by `Thread` to the UI. The pi backend produces the run
/// lifecycle subset; the remaining variants exist so the workspace and
/// conversation list compile against the shared contract and simply never
/// fire.
/// One streamed child-session observation from a running sub-agent (pi path:
/// bridged from the child's `AgentEvent`s through the Agent tool's progress
/// channel). Text/thinking deltas append to the drill-down transcript; tool
/// start/end render as activity lines.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentChildEvent {
    /// Assistant text delta from the child.
    Text(String),
    /// Assistant thinking delta from the child.
    Thinking(String),
    /// The child started a tool call; `id` is the child session's tool-call
    /// id so observers can pair start/end under parallel child execution.
    ToolStart {
        id: String,
        name: String,
        /// (argument key, truncated value) hint, e.g. `("path", "src/x.rs")`.
        hint: Option<(String, String)>,
    },
    /// The child's tool call finished.
    ToolEnd {
        id: String,
        name: String,
        is_error: bool,
    },
}

#[derive(Debug)]
pub enum ThreadEvent {
    /// Assistant text delta.
    AgentText(String),
    /// Assistant thinking delta.
    AgentThinking(String),
    /// Tool call status change.
    ToolCall {
        id: String,
        name: String,
        title: String,
        status: ToolCallStatus,
        input: Option<serde_json::Value>,
    },
    /// Tool execution result (output fed back to the model and shown in the UI).
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
    /// Live output chunk from a streaming tool (e.g. `bash` stdout/stderr).
    ToolOutput {
        id: String,
        chunk: String,
    },
    /// A sub-agent's child thread was constructed. Not produced by the pi
    /// backend in this stage (sub-agent observation panels are not wired).
    SubagentStarted {
        id: String,
        subagent_type: String,
        description: String,
        child: Entity<Thread>,
    },
    /// A spawned sub-agent's aggregated progress.
    SubagentProgress {
        id: String,
        subagent_type: String,
        tool_uses: u32,
        token_usage: TokenUsage,
        latest_activity: Option<String>,
        status: ToolCallStatus,
    },
    /// A streamed child-session event from a running sub-agent (the pi
    /// bridge of the child's text/thinking deltas and tool lifecycle). The
    /// conversation attaches these to the Agent tool call's drill-down
    /// output; the rail uses them for live activity.
    SubagentChild {
        id: String,
        child: SubagentChildEvent,
    },
    /// Request user authorization for a tool call: approval-gated tools
    /// escalate here, and `AskUserQuestion` rides the same channel. The
    /// workspace renders the question card and answers through
    /// [`Thread::respond_authorization`].
    ToolCallAuthorization {
        id: String,
        tool_name: String,
        summary: String,
        input: serde_json::Value,
    },
    /// An autopilot approval decision.
    ApprovalDecision {
        tool_name: String,
        tool_title: String,
        verdict: crate::approval::ReviewVerdict,
    },
    /// Approval mode changed.
    ApprovalModeChanged { mode: ApprovalMode },
    /// A completion turn started.
    TurnStarted,
    /// A completion turn ended.
    Stop(StopReason),
    /// The completion loop unwound and released the running slot.
    TurnFinished {
        cancelled: bool,
        failed: bool,
        stranded_steer_ids: Vec<String>,
    },
    /// The provider is retrying the HTTP handshake after a transient failure.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
        detail: Option<String>,
    },
    /// An error during streaming.
    Error(anyhow::Error),
    /// The prefix-stability fingerprint for this turn vs. the previous one.
    PrefixStability {
        stability_pct: u16,
        system_changed: bool,
        tools_changed: bool,
    },
    /// The provider-side prompt cache was lost since the previous turn.
    CacheInvalidation { reprocessed_tokens: u64 },
    /// Cumulative side-call breakdown.
    SideCallMetricsUpdated(Vec<SideCallMetric>),
    MainCallMetricsUpdated(SideCallMetric),
    /// Cumulative token usage changed.
    TokenUsageUpdated(TokenUsage),
    /// The user switched models mid-conversation.
    ModelChanged { from: Option<String>, to: String },
    /// Reasoning effort changed.
    ReasoningEffortChanged { effort: ReasoningEffort },
    /// Goal mode toggled on/off.
    GoalChanged { active: bool },
    /// Git-worktree binding changed: entered (worktree path) or exited.
    WorktreeChanged { active: bool, path: Option<String> },
    /// Auto-compaction summarization pass started.
    CompactionStarted { tokens_before: u64 },
    /// A compaction pass landed.
    Compaction {
        summary: String,
        messages_compacted: usize,
        tokens_before: u64,
    },
    /// The model submitted a plan file for the user's review verdict via
    /// the `ProposePlan` tool.
    PlanReady { plan_file: String, title: String },
    /// The model published/updated its execution task list via `UpdatePlan`;
    /// the context rail renders it as the plan overview.
    PlanUpdated {
        snapshot: crate::plan::PlanSnapshot,
    },
    /// Plan mode toggled (persisted in the session sidecar); the workspace
    /// mirrors it for the plan chip.
    PlanModeChanged { enabled: bool },
    /// A peer message was delivered from another team member.
    PeerMessage { from: String, content: String },
    /// A queued steer follow-up was drained into `messages`.
    SteerInjected { message_id: String },
    /// A page-state notification from a built-in browser tab.
    BrowserNotification {
        tab_id: crate::webview_host::BrowserTabId,
        notification: crate::webview_host::BrowserNotification,
    },
    /// An untrusted page requested an inbound write.
    InboundAuthorization {
        id: String,
        intent: String,
        payload: serde_json::Value,
    },
    /// A background task's state changed.
    BackgroundTaskUpdated {
        snapshot: TaskSnapshot,
    },
    /// The display-only history preview streamed another batch; the
    /// workspace appends the newly available messages to the conversation.
    ///
    /// parity: the backend emits this from `BackendNotice::HistoryProgress` —
    /// keep the two variants in sync when either side changes.
    HistoryProgress,
    /// The pi backend restored an existing session and the authoritative
    /// history is ready. The workspace rebuilds the conversation view.
    HistoryRestored,
}

/// The pi-backed thread facade.
pub struct Thread {
    pub id: ThreadId,
    cwd: PathBuf,
    project: Option<PathBuf>,
    model: Option<PiModel>,
    approval_mode: ApprovalMode,
    messages: Vec<Message>,
    reasoning_effort: ReasoningEffort,
    pinned: bool,
    archived: bool,
    running: bool,
    restored: bool,
    ui_notes: Vec<UiNoteRecord>,
    request_usage: HashMap<String, TokenUsage>,
    /// Text of user messages inserted since the last run, drained by
    /// `run_turn` into one prompt.
    pending_prompts: Vec<String>,
    /// Image blocks attached to the pending prompts, drained by `run_turn`
    /// onto the engine (kernel `ContentBlock::Image`).
    pending_images: Vec<pi::types::ContentBlock>,
    /// Steer message ids handed to the engine this run, awaiting settlement.
    pending_steers: VecDeque<String>,
    /// UI metadata of the most recently inserted user turn, re-attached to
    /// the authoritative history's last user message after each refresh.
    last_user_ui: Option<MessageUiMetadata>,
    /// The harness backend, materialized lazily for landing threads (see
    /// `Thread::landing` / `ensure_engine`).
    engine: Option<Arc<dyn ThreadEngine>>,
    /// History-loading state (see `HistoryPhase`).
    history_phase: HistoryPhase,
    /// Whether the user explicitly set the approval mode on a landing
    /// thread; the mode is then not overwritten by the session sidecar's
    /// default when the engine materializes.
    approval_mode_explicitly_set: bool,
    /// Plan mode active: read-only research + plan-file writes, proposals
    /// ride the `ProposePlan` tool. Mirrored from the engine on
    /// `PlanModeChanged`/`Ready`.
    plan_mode: bool,
    /// Last `UpdatePlan` snapshot mirrored from the engine: the rail's
    /// rebuild-after-compaction fallback (the transcript's plan tool calls
    /// are summarized away, but this survives via the session sidecar).
    persisted_plan: Option<crate::plan::PlanSnapshot>,
    /// Member label for team routing: `lead` for the main thread, the
    /// member name for team workers.
    label: String,
    /// The team this thread belongs to. The leader owns the `Entity<Team>`;
    /// members hold the same entity (set at spawn, cleared on disband).
    team: Option<Entity<Team>>,
    /// Shared goal state with the engine's goal tools; `None` only when the
    /// threads db is unavailable (goal features degrade off).
    goal_bridge: Option<Arc<GoalBridge>>,    worktree_path: Option<String>,
}

impl EventEmitter<ThreadEvent> for Thread {}

impl Thread {
    /// The startup landing state: a detached thread with no engine. No
    /// session is loaded at launch — the user picks a conversation from the
    /// sidebar (`open_existing` swaps in its engine) or starts typing
    /// (`run_turn` materializes a fresh engine on first use).
    pub fn landing(cwd: PathBuf, cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            id: ThreadId(uuid::Uuid::new_v4().to_string()),
            cwd,
            project: None,
            model: crate::pi_providers::default_model(),
            approval_mode: ApprovalMode::default(),
            messages: Vec::new(),
            reasoning_effort: ReasoningEffort::default(),
            pinned: false,
            archived: false,
            running: false,
            restored: false,
            ui_notes: Vec::new(),
            request_usage: HashMap::new(),
            pending_prompts: Vec::new(),
            pending_images: Vec::new(),
            pending_steers: VecDeque::new(),
            last_user_ui: None,
            engine: None,
            history_phase: HistoryPhase::Ready,
            approval_mode_explicitly_set: false,
            plan_mode: false,
            persisted_plan: None,
            label: "lead".into(),
            team: None,
            goal_bridge: None,            worktree_path: None,
        })
    }

    /// A genuinely empty thread (sidebar new-conversation): never restores
    /// the previous session.
    pub fn new_fresh(id: ThreadId, cwd: PathBuf, cx: &mut App) -> Entity<Self> {
        Self::open(id, cwd, None, None, true, cx)
    }

    /// Construct a thread bound to a project directory: a fresh session with
    /// the project as its cwd in one step (no recreate, no restore), so the
    /// sidebar never sees an orphaned pre-project session file.
    pub fn new_in_project(id: ThreadId, project: PathBuf, cx: &mut App) -> Entity<Self> {
        Self::open(id, project.clone(), None, Some(project), true, cx)
    }

    /// Construct a thread backed by a specific session file (sidebar open).
    pub fn open_existing(id: ThreadId, cwd: PathBuf, path: PathBuf, cx: &mut App) -> Entity<Self> {
        Self::open(id, cwd, Some(path), None, false, cx)
    }

    fn open(
        id: ThreadId,
        cwd: PathBuf,
        initial_path: Option<PathBuf>,
        project: Option<PathBuf>,
        fresh: bool,
        cx: &mut App,
    ) -> Entity<Self> {
        // A concrete session file means an authoritative restore is pending;
        // the facade reports `Loading` until `Ready` so the workspace can
        // gate input and render the streaming preview.
        let loading = initial_path.is_some();
        let model = crate::pi_providers::default_model();
        let sessions_dir = crate::paths::manox_config_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("pi-sessions");
        // Goal bridge seeds from the persisted goal (restore path) and is
        // shared with the engine's goal tools; db unavailability degrades
        // goal features off rather than blocking the thread.
        let goal_bridge = GoalBridge::for_thread(&id.0);
        let SpawnedEngine { engine, events } = crate::pi_engine::spawn_engine(
            cwd.clone(),
            model.clone(),
            sessions_dir,
            initial_path,
            fresh,
            project.clone(),
            id.0.clone(),
            goal_bridge.clone(),
        );

        cx.new(|cx| {
            drain_engine_notices(cx, events);
            Self {
                id,
                cwd,
                project,
                model,
                approval_mode: ApprovalMode::default(),
                messages: Vec::new(),
                reasoning_effort: ReasoningEffort::default(),
                pinned: false,
                archived: false,
                running: false,
                restored: false,
                ui_notes: Vec::new(),
                request_usage: HashMap::new(),
                pending_prompts: Vec::new(),
                pending_images: Vec::new(),
                pending_steers: VecDeque::new(),
                last_user_ui: None,
                engine: Some(engine),
                history_phase: if loading {
                    HistoryPhase::Loading
                } else {
                    HistoryPhase::Ready
                },
                approval_mode_explicitly_set: false,
            plan_mode: false,
            persisted_plan: None,
                label: "lead".into(),
                team: None,
                goal_bridge,            worktree_path: None,
            }
        })
    }

    /// Lazily materialize the engine for a landing thread (no engine until
    /// the user acts: a sidebar open swaps the whole thread via
    /// `open_existing` instead; a first prompt or project bind calls this).
    /// Spawns a fresh session (never restores) bound to `project`, wires the
    /// notice drainer, and replays the stored approval mode / reasoning
    /// effort. `spawn_engine` is infallible (it only queues the actor), so
    /// the engine is always available after this returns.
    fn ensure_engine(&mut self, project: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.engine.is_some() {
            return;
        }
        if self.goal_bridge.is_none() {
            self.goal_bridge = GoalBridge::for_thread(&self.id.0);
        }
        let cwd = project.clone().unwrap_or_else(|| self.cwd.clone());
        let model = self.model.clone();
        let sessions_dir = crate::paths::manox_config_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("pi-sessions");
        let SpawnedEngine { engine, events } = crate::pi_engine::spawn_engine(
            cwd.clone(),
            model,
            sessions_dir,
            None,
            true,
            project,
            self.id.0.clone(),
            self.goal_bridge.clone(),
        );
        if self.approval_mode != ApprovalMode::default() {
            engine.set_approval_mode(self.approval_mode);
        }
        if self.reasoning_effort != ReasoningEffort::default() {
            engine.set_thinking_level(Some(self.reasoning_effort.wire_value().to_string()));
        }
        self.engine = Some(engine.clone());
        drain_engine_notices(cx, events);
    }

    /// Handle one backend notice on the gpui thread: mirror state and re-emit
    /// the UI-facing event.
    fn handle_notice(&mut self, notice: BackendNotice, cx: &mut Context<Self>) {
        match notice {
            BackendNotice::Event(event) => {
                // Mirror the gate policy before the chip hears about the
                // change.
                if let ThreadEvent::ApprovalModeChanged { mode } = *event {
                    self.approval_mode = mode;
                }
                if let ThreadEvent::PlanModeChanged { enabled } = *event {
                    self.plan_mode = enabled;
                }
                if let ThreadEvent::PlanUpdated { snapshot } = &*event {
                    // Mirror + persist: the sidecar copy is the rail's
                    // rebuild source after compaction summarizes the
                    // transcript's UpdatePlan calls away. Empty snapshot =
                    // the model dropped its plan → clear the persisted copy.
                    let persisted = (!snapshot.is_empty()).then(|| snapshot.clone());
                    self.persisted_plan = persisted.clone();
                    if let Some(engine) = &self.engine {
                        engine.persist_plan_snapshot(
                            persisted
                                .as_ref()
                                .and_then(|p| serde_json::to_value(p).ok()),
                        );
                    }
                }
                // Runs the actor starts on its own (monitor idle-wakeups,
                // plan-approval seeds) announce themselves with `TurnStarted`;
                // mirror them onto the running flag so a switch-away parks the
                // thread instead of dropping it mid-run. Idempotent with the
                // facade's own `run_turn` (which emits `TurnStarted`
                // synchronously before the engine picks up the prompt).
                if matches!(&*event, ThreadEvent::TurnStarted) {
                    self.running = true;                if let ThreadEvent::WorktreeChanged { active, path } = &*event {
                    self.worktree_path = if *active { path.clone() } else { None };
                }
                }
                cx.emit(*event);
            }
            BackendNotice::LiveHistory => {
                // Mid-run mirror refresh (no UI event): the workspace's
                // switch-back rebuild reads `messages()`, so a thread parked
                // while still generating shows current progress instead of the
                // last settled snapshot. The authoritative `Settled` refresh
                // replaces this mirror.
                self.refresh_history(cx);
            }
            BackendNotice::TeamRequest { op, responder } => {
                // Team state is gpui-side (entities); execute the op here
                // and reply through the tool's responder channel.
                let result = crate::team::tools::execute_team_op(self, op, cx);
                let _ = responder.try_send(result);
            }
            BackendNotice::BrowserRequest { op, responder } => {
                // The browser host is a gpui main-thread surface; the tool
                // (tokio) parked its request here. Execute on the main
                // thread and reply through the responder channel.
                let Some(host) = crate::webview_host::host().cloned() else {
                    let _ = responder.try_send(Err("browser host not available".to_string()));
                    return;
                };
                use crate::thread_engine::{BrowserOp, BrowserReply};
                cx.spawn(async move |_this, cx: &mut gpui::AsyncApp| {
                    let result: Result<BrowserReply, String> = match op {
                        BrowserOp::Open { url } => {
                            cx.update(|cx| host.open_tab(&url, cx).map(BrowserReply::TabId))
                        }
                        BrowserOp::Navigate { id, url } => {
                            cx.update(|cx| host.navigate(id, &url, cx).map(|_| BrowserReply::Unit))
                        }
                        BrowserOp::Close { id } => {
                            cx.update(|cx| host.close_tab(id, cx).map(|_| BrowserReply::Unit))
                        }
                        BrowserOp::ReadText { id } => {
                            let task = cx.update(|cx| host.read_text(id, cx));
                            task.await.map(BrowserReply::Text)
                        }
                        BrowserOp::ReadDom { id, selector } => {
                            let task = cx.update(|cx| host.read_dom(id, selector, cx));
                            task.await.map(BrowserReply::Text)
                        }
                        BrowserOp::Click { id, selector } => {
                            let task = cx.update(|cx| host.click(id, &selector, cx));
                            task.await.map(|_| BrowserReply::Unit)
                        }
                        BrowserOp::TypeText { id, selector, text } => {
                            let task = cx.update(|cx| host.type_text(id, &selector, &text, cx));
                            task.await.map(|_| BrowserReply::Unit)
                        }
                        BrowserOp::Scroll { id, dx, dy } => {
                            let task = cx.update(|cx| host.scroll(id, dx, dy, cx));
                            task.await.map(|_| BrowserReply::Unit)
                        }
                        BrowserOp::Screenshot { id } => {
                            let task = cx.update(|cx| host.screenshot(id, cx));
                            task.await.map(BrowserReply::Text)
                        }
                        BrowserOp::YieldToUser { id } => {
                            let task = cx.update(|cx| host.yield_to_user(id, cx));
                            task.await.map(|_| BrowserReply::Unit)
                        }
                    };
                    let _ = responder.send(result).await;
                })
                .detach();
            }
            BackendNotice::Ready {
                restored,
                model,
                approval_mode,
                plan_mode,
                plan_file,
                plan_review_pending,
                plan_snapshot,
            } => {
                // Unconditional: a session switch must drop the previous
                // session's plan when the opened session has none.
                self.persisted_plan = plan_snapshot
                    .and_then(|v| serde_json::from_value(v).ok());
                self.restored = restored;
                if let Some(m) = model {
                    self.model = Some(m);
                }
                if !self.approval_mode_explicitly_set {
                    self.approval_mode = approval_mode;
                }
                if plan_mode {
                    self.plan_mode = true;
                }
                if plan_review_pending
                    && let Some(plan_file) = plan_file
                {
                    // Re-surface the pending review card after a restart:
                    // re-read the plan file and resolve the title the same
                    // way the live propose did.
                    let content = std::fs::read_to_string(&plan_file).unwrap_or_default();
                    let slug = std::path::Path::new(&plan_file)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .trim_end_matches("-plan")
                        .to_string();
                    let title = crate::plan_mode::resolve_plan_title(None, &content, &slug);
                    cx.emit(ThreadEvent::PlanReady { plan_file, title });
                }
                let was_loading = self.history_phase.is_loading();
                self.history_phase = HistoryPhase::Ready;
                self.refresh_history(cx);
                // Rebuild on restore, and also after a failed restore — the
                // preview may have streamed a corrupt file's partial content
                // and the fresh fallback session is empty, so the workspace
                // must return to the hero. Fresh threads skip the rebuild
                // (nothing changed since attach).
                if restored || was_loading {
                    cx.emit(ThreadEvent::HistoryRestored);
                }
            }
            BackendNotice::HistoryProgress => {
                if self.history_phase.is_loading() {
                    self.refresh_history(cx);
                    cx.emit(ThreadEvent::HistoryProgress);
                }
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
                self.running = false;
                self.pending_steers.clear();
                self.refresh_history(cx);
                cx.emit(ThreadEvent::TurnFinished {
                    cancelled,
                    failed,
                    stranded_steer_ids: stranded,
                });
            }
            BackendNotice::Fatal(err) => {
                self.running = false;
                // The actor will not send `Ready` (it bailed out): clear the
                // loading phase so the workspace leaves the spinner and the
                // input gate opens. Any preview content is stale — the
                // session never assembled — so drop it and rebuild to the
                // hero (the error surfaces as a conversation notice).
                self.history_phase = HistoryPhase::Ready;
                self.messages.clear();
                cx.emit(ThreadEvent::HistoryRestored);
                cx.emit(ThreadEvent::Error(err));
            }
            BackendNotice::SessionListDirty => {
                let store = crate::thread_store::global();
                store.update(cx, |s, cx| s.refresh(cx));
            }
        }
    }

    /// Restore the bound project from a reopened session's sidecar without
    /// recreating the session (used by the store on load).
    pub fn restore_project(&mut self, dir: PathBuf) {
        self.cwd = dir.clone();
        self.project = Some(dir);
    }

    /// Replace the mirrored history with the engine's authoritative transcript
    /// and re-attach the last user prompt's UI metadata. Tool results also use
    /// the User role on the provider wire but must never inherit prompt chrome.
    /// No-op while the engine is not materialized (a landing thread has no
    /// backend history).
    fn refresh_history(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = &self.engine else {
            return;
        };
        let mut mapped = engine.history();
        if let Some(ui) = self.last_user_ui.clone()
            && let Some(last_user) = mapped
                .iter_mut()
                .rev()
                .find(|m| {
                    matches!(m.role, crate::language_model::Role::User)
                        && m.provenance == crate::message::MessageProvenance::User
                })
        {
            last_user.ui = Some(ui);
        }
        self.messages = mapped;
        self.request_usage = engine.request_token_usage();
        cx.notify();
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
        // Text blocks join the prompt text; image blocks ride the next
        // prompt as kernel `ContentBlock::Image` (TS `prompt(text, { images })`
        // parity).
        let mut images = Vec::new();
        let text: String = content
            .iter()
            .filter_map(|c| match c {
                MessageContent::Text(t) => Some(t.as_str()),
                MessageContent::Image { data, mime_type } => {
                    images.push(pi::types::ContentBlock::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    });
                    None
                }
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
        if !images.is_empty() {
            self.pending_images.extend(images);
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
        let mut images = Vec::new();
        let text: String = content
            .iter()
            .filter_map(|c| match c {
                MessageContent::Text(t) => Some(t.as_str()),
                MessageContent::Image { data, mime_type } => {
                    images.push(pi::types::ContentBlock::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    });
                    None
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut message = Message::user_with_content(content);
        message.ui = ui;
        let id = message.id.clone();
        self.pending_steers.push_back(id.clone());
        if let Some(engine) = &self.engine {
            engine.steer(text, images);
        }
        cx.notify();
        // The canonical message joins history at the next refresh (pi owns the
        // transcript); the workspace renders the optimistic bubble until
        // `SteerInjected` confirms.
        id
    }

    pub fn run_turn(&mut self, cx: &mut Context<Self>) {
        if self.running || (self.pending_prompts.is_empty() && self.pending_images.is_empty()) {
            return;
        }
        self.ensure_engine(self.project.clone(), cx);
        let prompt = std::mem::take(&mut self.pending_prompts).join("\n\n");
        let images = std::mem::take(&mut self.pending_images);
        self.running = true;
        cx.emit(ThreadEvent::TurnStarted);
        self.engine
            .as_ref()
            .expect("ensure_engine materialized the engine")
            .run(prompt, images);
        cx.notify();
    }

    pub fn cancel(&mut self, _cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.abort();
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Test-only: force the running flag so team routing tests can simulate
    /// a busy thread without a live engine turn.
    #[cfg(test)]
    pub(crate) fn set_running_for_test(&mut self, running: bool) {
        self.running = running;
    }

    /// Test-only: set the member label.
    #[cfg(test)]
    pub(crate) fn set_label_for_test(&mut self, label: String) {
        self.label = label;
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

    pub fn model(&self) -> Option<&PiModel> {
        self.model.as_ref()
    }

    pub fn display_title(&self) -> String {
        // Mechanical summary like the manox build's fallback: the first user
        // prompt, trimmed to a title-sized prefix.
        self.messages
            .iter()
            .find(|m| matches!(m.role, crate::language_model::Role::User))
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    MessageContent::Text(t) if !t.trim().is_empty() => Some(t.trim()),
                    _ => None,
                })
            })
            .map(|t| {
                let flat: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
                crate::title::initial_title(&flat).unwrap_or_default()
            })
            .unwrap_or_else(|| "Manox Pi".to_string())
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.reasoning_effort
    }

    pub fn agent_language(&self) -> Language {
        crate::settings::load().resolve().agent
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
            .find(|m| matches!(m.role, crate::language_model::Role::User))
            .map(|m| m.id.as_str())
    }

    pub fn has_interacted(&self) -> bool {
        self.messages
            .iter()
            .any(|m| matches!(m.role, crate::language_model::Role::User))
    }

    pub fn request_token_usage(&self) -> &HashMap<String, TokenUsage> {
        &self.request_usage
    }

    pub fn last_request_token_usage(&self) -> Option<TokenUsage> {
        None
    }

    pub fn cumulative_token_usage(&self) -> TokenUsage {
        self.engine
            .as_ref()
            .map(|e| e.cumulative_token_usage())
            .unwrap_or_default()
    }

    pub fn per_model_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.engine
            .as_ref()
            .map(|e| e.per_model_token_usage())
            .unwrap_or_default()
    }

    pub fn cumulative_cost(&self) -> f64 {
        self.engine.as_ref().map(|e| e.cumulative_cost()).unwrap_or(0.0)
    }

    pub fn per_model_cost(&self) -> HashMap<String, f64> {
        self.engine
            .as_ref()
            .map(|e| e.per_model_cost())
            .unwrap_or_default()
    }

    pub fn ui_notes(&self) -> &[UiNoteRecord] {
        &self.ui_notes
    }

    pub fn worktree(&self) -> Option<&WorktreeState> {
        None
    }

    /// The thread's persisted Goal — one shared snapshot with the engine's
    /// goal tools (model-side writes land here too).
    pub fn goal(&self) -> Option<ThreadGoal> {
        self.goal_bridge.as_ref().and_then(|bridge| bridge.snapshot())
    }

    /// Elapsed seconds shown on the goal chip: accumulated accounting plus
    /// wall clock since creation while the goal is live. Per-turn token/time
    /// accounting is a follow-up, so `time_used_seconds` stays 0 and the
    /// display is creation-anchored wall time for non-terminal goals.
    pub fn goal_elapsed_seconds(&self) -> Option<u64> {
        let goal = self.goal()?;
        let live = if goal.status.is_terminal() {
            0
        } else {
            let now = chrono::Utc::now().timestamp();
            (now - goal.created_at).max(0) as u64
        };
        Some(goal.time_used_seconds.saturating_add(live))
    }

    fn goal_bridge_or_bail(&self) -> anyhow::Result<Arc<GoalBridge>> {
        self.goal_bridge
            .clone()
            .ok_or_else(|| anyhow::anyhow!("goal store unavailable"))
    }

    /// User-side Goal creation (`/goal <objective>` or the model's
    /// `CreateGoal` tool both land on the shared bridge).
    pub fn create_goal(
        &mut self,
        objective: String,
        token_budget: Option<u64>,
        actor: crate::db::GoalActor,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        bridge.create_goal(objective, token_budget, actor)?;
        self.ensure_engine(self.project.clone(), cx);
        if let Some(engine) = &self.engine {
            engine.goal_started();
        }
        cx.emit(ThreadEvent::GoalChanged { active: true });
        Ok(())
    }

    /// Convenience for `/goal <objective>`: create with no budget as the
    /// user.
    pub fn set_goal(&mut self, objective: String, cx: &mut Context<Self>) -> anyhow::Result<()> {
        self.create_goal(objective, None, crate::db::GoalActor::User, cx)
    }

    /// Edit objective/budget in place (keeps id and status).
    pub fn edit_goal(
        &mut self,
        objective: String,
        token_budget: Option<u64>,
        actor: crate::db::GoalActor,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        let goal = bridge.edit_goal(objective, token_budget, actor)?;
        cx.emit(ThreadEvent::GoalChanged {
            active: !goal.status.is_terminal(),
        });
        Ok(())
    }

    /// Replace an unfinished Goal with a fresh one (explicit `/goal
    /// replace` confirmation path).
    pub fn replace_goal(
        &mut self,
        objective: String,
        token_budget: Option<u64>,
        actor: crate::db::GoalActor,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        bridge.replace_goal(objective, token_budget, actor)?;
        self.ensure_engine(self.project.clone(), cx);
        if let Some(engine) = &self.engine {
            engine.goal_started();
        }
        cx.emit(ThreadEvent::GoalChanged { active: true });
        Ok(())
    }

    /// Pause/resume/blocked transitions with the domain guards.
    pub fn set_goal_status(
        &mut self,
        status: crate::goal::GoalStatus,
        reason: Option<String>,
        actor: crate::db::GoalActor,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        let goal = bridge.set_goal_status(status, reason, actor)?;
        cx.emit(ThreadEvent::GoalChanged {
            active: !goal.status.is_terminal(),
        });
        Ok(())
    }

    /// Clear the current Goal (row deleted, audit trail kept).
    pub fn clear_goal(
        &mut self,
        actor: crate::db::GoalActor,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        bridge.clear_goal(actor)?;
        cx.emit(ThreadEvent::GoalChanged { active: false });
        Ok(())
    }

    pub fn depth(&self) -> u32 {
        0
    }

    pub fn agent_label(&self) -> &str {
        &self.label
    }

    /// The team this thread belongs to (leader owns; member holds).
    pub fn team(&self) -> Option<&Entity<Team>> {
        self.team.as_ref()
    }

    /// Attach this thread to a team (leader at `TeamCreate`, member at
    /// spawn).
    pub fn set_team(&mut self, team: Entity<Team>, _cx: &mut Context<Self>) {
        self.team = Some(team);
    }

    /// Detach from the team (disband path).
    pub fn clear_team(&mut self, _cx: &mut Context<Self>) {
        self.team = None;
    }

    /// Deliver peer messages from teammates: render each through the peer
    /// wrapper template, emit `PeerMessage`, then run a turn over the batch
    /// (no-op when the engine is mid-turn — the team queues instead).
    pub fn deliver_peer_messages(
        &mut self,
        msgs: Vec<crate::team::PeerMessage>,
        cx: &mut Context<Self>,
    ) {
        if msgs.is_empty() {
            return;
        }
        for msg in &msgs {
            let rendered = crate::prompt::render(
                crate::prompt::PromptTemplate::WrapperPeerMessage,
                self.agent_language(),
                &crate::prompt::PeerMessageData {
                    from: msg.from.clone(),
                    content: msg.content.clone(),
                },
            )
            .unwrap_or_else(|_| format!("[from {}] {}", msg.from, msg.content));
            self.insert_user_message_with_ui_metadata(rendered, None, cx);
            cx.emit(ThreadEvent::PeerMessage {
                from: msg.from.clone(),
                content: msg.content.clone(),
            });
        }
        self.run_turn(cx);
    }

    /// Construct a team worker thread: a fresh pi session inheriting this
    /// (leader) thread's cwd / model / approval mode / reasoning effort,
    /// labeled with the member name. Engine spawned eagerly (members always
    /// run). Members carry no goal bridge: the goal contract belongs to the
    /// leader's user-facing conversation.
    pub fn new_team_member(&self, name: String, cx: &mut App) -> Entity<Self> {
        let id = ThreadId(uuid::Uuid::new_v4().to_string());
        let cwd = self.cwd.clone();
        let model = self.model.clone();
        let approval_mode = self.approval_mode;
        let reasoning_effort = self.reasoning_effort;
        let sessions_dir = crate::paths::manox_config_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("pi-sessions");
        let SpawnedEngine { engine, events } = crate::pi_engine::spawn_engine(
            cwd.clone(),
            model.clone(),
            sessions_dir,
            None,
            true,
            None,
            id.0.clone(),
            None,
        );
        if approval_mode != ApprovalMode::default() {
            engine.set_approval_mode(approval_mode);
        }
        if reasoning_effort != ReasoningEffort::default() {
            engine.set_thinking_level(Some(reasoning_effort.wire_value().to_string()));
        }
        cx.new(|cx| {
            drain_engine_notices(cx, events);
            Self {
                id,
                cwd,
                project: None,
                model,
                approval_mode,
                messages: Vec::new(),
                reasoning_effort,
                pinned: false,
                archived: false,
                running: false,
                restored: false,
                ui_notes: Vec::new(),
                request_usage: HashMap::new(),
                pending_prompts: Vec::new(),
                pending_images: Vec::new(),
                pending_steers: VecDeque::new(),
                last_user_ui: None,
                engine: Some(engine),
                history_phase: HistoryPhase::Ready,
                approval_mode_explicitly_set: true,
                plan_mode: false,
                persisted_plan: None,
                goal_bridge: None,
                label: name,
                team: None,
                worktree_path: None,
            }
        })
    }
    pub fn background_task_snapshots(&self) -> Vec<TaskSnapshot> {
        Vec::new()
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for m in &self.messages {
            let heading = match m.role {
                crate::language_model::Role::User => "## User",
                crate::language_model::Role::Assistant => "## Assistant",
                crate::language_model::Role::System => "## System",
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

    // ── Thread duck-type: setters ──────────────────────────────────────────

    /// Seed the approved plan as the next user message without running it
    /// (the clear-context verdict inserts on a fresh thread, then the
    /// workspace launches the turn).
    /// Seed an approved plan's execution on this thread and run it: the
    /// rendered execution directive (referencing the plan file) becomes the
    /// first user message. Used by the fresh-context verdict on the spawned
    /// thread.
    pub fn seed_plan_execution(
        &mut self,
        plan_file: String,
        seed_text: String,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        self.ensure_engine(self.project.clone(), cx);
        if let Some(engine) = &self.engine {
            engine.start_plan_execution(plan_file);
        }
        self.insert_user_message_with_ui_metadata(seed_text, ui, cx);
        self.run_turn(cx);
    }

    /// Plan mode active for this thread (mirrored from the engine).
    pub fn plan_mode(&self) -> bool {
        self.plan_mode
    }

    /// Last `UpdatePlan` snapshot mirrored from the engine (sidecar-backed):
    /// the rail's rebuild fallback once compaction has summarized the
    /// transcript's plan tool calls away.
    pub fn persisted_plan(&self) -> Option<&crate::plan::PlanSnapshot> {
        self.persisted_plan.as_ref()
    }

    /// Active git-worktree path for this thread (mirrored from the engine);
    /// `None` when the session runs in its original directory.
    pub fn worktree_path(&self) -> Option<&str> {
        self.worktree_path.as_deref()
    }

    /// Persist whether a plan review card is pending, so a restarted
    /// session re-surfaces the card (the card itself is UI-only state).
    pub fn set_plan_review_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.set_plan_review_pending(pending);
        }
        let _ = cx;
    }

    /// Toggle plan mode: the engine persists the flag in the session
    /// sidecar, wires the read-only gate, and injects the plan-mode
    /// instructions (rendered for the configured agent language) every turn.
    pub fn set_plan_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.plan_mode = enabled;
        if let Some(engine) = &self.engine {
            engine.set_plan_mode(enabled);
        }
        cx.notify();
    }

    /// Execute an approved plan on this thread: optionally compact the
    /// planning context first (distilled toward the plan file), then run
    /// the execution seed turn.
    pub fn approve_plan(
        &mut self,
        compact: bool,
        compact_instructions: Option<String>,
        seed_text: String,
        cx: &mut Context<Self>,
    ) {
        self.plan_mode = false;
        if let Some(engine) = &self.engine {
            engine.approve_plan(compact, compact_instructions, seed_text);
        }
        cx.notify();
    }

    pub fn set_project(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.has_interacted() {
            return;
        }
        self.cwd = dir.clone();
        self.project = Some(dir.clone());
        if let Some(engine) = &self.engine {
            engine.new_session(dir.clone(), Some(dir));
        } else {
            // Landing thread: materialize a project-bound fresh engine in
            // one step (no orphaned pre-project session file).
            self.ensure_engine(Some(dir), cx);
        }
        cx.notify();
    }

    /// Manual compaction (`/compact`): no-op while a turn is in flight (the
    /// kernel compacts an idle transcript only); the recap card lands via the
    /// engine's harness-event adaptation.
    pub fn compact(&mut self, custom_instructions: Option<String>, _cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        if let Some(engine) = &self.engine {
            engine.compact(custom_instructions);
        }
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        if self.approval_mode == mode {
            return;
        }
        self.approval_mode = mode;
        // An explicit user choice must survive engine materialization: the
        // session sidecar's default would otherwise overwrite it at `Ready`.
        self.approval_mode_explicitly_set = true;
        if let Some(engine) = &self.engine {
            // The engine applies the mode to its gate and persists it in the
            // session sidecar; the chip reflects the change immediately.
            engine.set_approval_mode(mode);
        }
        cx.emit(ThreadEvent::ApprovalModeChanged { mode });
        cx.notify();
    }

    /// Deliver the user's verdict for a pending tool-call authorization
    /// (approval card or `AskUserQuestion`). Unknown ids are ignored.
    pub fn respond_authorization(
        &mut self,
        id: &str,
        response: crate::permission::ToolAuthorizationResponse,
        cx: &mut Context<Self>,
    ) {
        // Composite ids (`<member>::<child_id>`) route the leader's verdict
        // to the member thread that owns the actual authorization.
        if id.contains("::")
            && let Some(team) = self.team.clone()
        {
            let resolved = team.read_with(cx, |t, _| t.resolve_child_auth(id));
            if let Some((member, child_id)) = resolved {
                member.update(cx, |m, cx| m.respond_authorization(&child_id, response, cx));
                team.update(cx, |t, _| t.clear_child_auth(id));
                return;
            }
        }
        if let Some(engine) = &self.engine {
            engine.respond_tool_authorization(id, response);
        }
    }

    /// Pending authorizations with their card metadata, so the workspace can
    /// re-surface a card after switching back to this thread.
    pub fn pending_auth_entries(&self) -> Vec<(String, crate::permission::PendingAuthMeta)> {
        self.engine
            .as_ref()
            .map(|e| e.pending_auth_entries())
            .unwrap_or_default()
    }

    pub fn set_pinned(&mut self, pinned: bool, cx: &mut Context<Self>) {
        self.pinned = pinned;
        cx.notify();
    }

    pub fn set_model(&mut self, model: PiModel, cx: &mut Context<Self>) {
        let from = self.model.as_ref().map(|m| m.id.clone());
        let to = model.id.clone();
        self.model = Some(model.clone());
        if let Some(engine) = &self.engine {
            engine.set_model(model);
        }
        cx.emit(ThreadEvent::ModelChanged { from, to });
        cx.notify();
    }

    pub fn set_reasoning_effort(&mut self, effort: ReasoningEffort, cx: &mut Context<Self>) {
        if self.reasoning_effort == effort {
            return;
        }
        self.reasoning_effort = effort;
        if let Some(engine) = &self.engine {
            engine.set_thinking_level(Some(effort.wire_value().to_string()));
        }
        cx.emit(ThreadEvent::ReasoningEffortChanged { effort });
        cx.notify();
    }

    pub fn set_archived(&mut self, archived: bool, cx: &mut Context<Self>) {
        self.archived = archived;
        cx.notify();
    }
}

/// Drain a spawned engine's notice channel on the gpui thread, dispatching
/// each notice through `Thread::handle_notice`. Shared by `open` (engine
/// present at construction) and `ensure_engine` (landing materialization).
fn drain_engine_notices(
    this: &mut Context<Thread>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
) {
    this.spawn(async move |this, cx| {
        while let Some(notice) = events.recv().await {
            let ok = this.update(cx, |t: &mut Thread, cx| t.handle_notice(notice, cx)).is_ok();
            if !ok {
                break;
            }
        }
    })
    .detach();
}

/// Human-readable tool card title from the pi tool name + arguments. The
/// third parameter (manox's sub-agent description override) is unused by the
/// pi backend, which never spawns manox sub-agents.
pub fn tool_title(name: &str, args: &serde_json::Value, _desc: Option<&str>) -> String {
    crate::pi_engine::adapt::tool_title(name, args)
}

// Shared helpers the compact/estimation path calls with the same signature as
// the manox build. The pi backend owns its own context management, so the
// model-facing mapping is identity here and the pure helpers mirror the
// manox semantics.

/// The model-facing form of one content block. Pi keeps blocks verbatim —
/// there is no manox envelope/compaction rewriting to undo.
pub fn model_facing_content(
    c: &MessageContent,
    _lang: crate::language::Language,
) -> MessageContent {
    c.clone()
}

impl Thread {
    pub fn pending_steer_ids(&self) -> Vec<String> {
        self.pending_steers.iter().cloned().collect()
    }

    pub fn cancel_pending_steer(&mut self, id: &str) -> bool {
        if let Some(pos) = self.pending_steers.iter().position(|s| s == id) {
            self.pending_steers.remove(pos);
            if let Some(engine) = &self.engine {
                engine.cancel_steer(id);
            }
            true
        } else {
            false
        }
    }

    pub fn push_ui_note(&mut self, note: UiNoteRecord) {
        self.ui_notes.push(note);
    }

    /// Run a markdown prompt-macro turn (`/plugin:command args`): render the
    /// command body with `$ARGUMENTS` substituted and send it as a user turn.
    /// The retired manox harness additionally applies the macro's
    /// `allowed-tools` filter for the turn; the pi harness runs its full
    /// toolset.
    pub fn submit_command(
        &mut self,
        name: &str,
        args: &str,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(cmd) = crate::command::global().get(name).cloned() else {
            return false;
        };
        self.insert_slash_turn(cmd.render(args), ui, cx);
        true
    }

    /// Run a skill turn: inject the named skill's body (description + body,
    /// the user's args appended) as the user message, mirroring the retired
    /// manox harness's `submit_skill`.
    pub fn submit_skill(
        &mut self,
        key: &str,
        args: &str,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(skill) = crate::skill::global().get(key).cloned() else {
            return false;
        };
        let rendered = crate::prompt::render(
            crate::prompt::PromptTemplate::SkillBody,
            self.agent_language(),
            &crate::prompt::SkillBodyData {
                description: (!skill.description.is_empty()).then(|| skill.description.clone()),
                body: skill.body.clone(),
                arguments: (!args.is_empty()).then(|| args.to_string()),
            },
        )
        .expect("skill body render");
        self.insert_slash_turn(rendered, ui, cx);
        true
    }

    /// Run a built-in slash command (`/danger`, `/plan`, `/compact`,
    /// `/goal`) at the thread level — the headless twin of the gpui host's
    /// built-in command impls, sharing the name/alias set via
    /// [`crate::slash_builtins`]. Returns `false` for names this layer does
    /// not own (`exit`/`new` are session-level and handled by the host).
    /// Prompt forms insert `args` as a user turn carrying the raw
    /// `/name args` display form, mirroring [`Self::submit_command`].
    pub fn run_slash_builtin(
        &mut self,
        name: &str,
        args: &str,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) -> bool {
        match crate::slash_builtins::canonical_builtin(name).map(|meta| meta.name) {
            Some("danger") => {
                if args.trim().is_empty() {
                    let next = match self.approval_mode() {
                        ApprovalMode::Danger => ApprovalMode::AutoPilot,
                        _ => ApprovalMode::Danger,
                    };
                    self.set_approval_mode(next, cx);
                } else {
                    if self.approval_mode() != ApprovalMode::Danger {
                        self.set_approval_mode(ApprovalMode::Danger, cx);
                    }
                    self.insert_slash_turn(args.to_string(), ui, cx);
                }
                true
            }
            Some("plan") => {
                if self.plan_mode() {
                    self.set_plan_mode(false, cx);
                } else {
                    self.set_plan_mode(true, cx);
                    if !args.trim().is_empty() {
                        self.insert_slash_turn(args.to_string(), ui, cx);
                    }
                }
                true
            }
            Some("compact") => {
                let trimmed = args.trim();
                let instructions = (!trimmed.is_empty()).then(|| trimmed.to_string());
                self.compact(instructions, cx);
                true
            }
            Some("goal") => {
                self.run_goal_slash(args, ui, cx);
                true
            }
            _ => false,
        }
    }

    /// `/goal` subcommand dispatch. Thread-state transitions mirror the gpui
    /// host's `GoalCommand`; the popover-only forms (bare `/goal`, bare
    /// `edit`/`replace`) are no-ops here — the webview surfaces goal state
    /// through its info card instead.
    fn run_goal_slash(&mut self, args: &str, ui: Option<MessageUiMetadata>, cx: &mut Context<Self>) {
        let trimmed = args.trim();
        if let Some(objective) = trimmed.strip_prefix("replace ").map(str::trim) {
            if let Err(error) = self.replace_goal(objective.to_string(), None, crate::db::GoalActor::User, cx)
            {
                cx.emit(ThreadEvent::Error(error));
            }
            return;
        }
        if let Some(objective) = trimmed.strip_prefix("edit ").map(str::trim) {
            let budget = self.goal().and_then(|goal| goal.token_budget);
            if let Err(error) = self.edit_goal(objective.to_string(), budget, crate::db::GoalActor::User, cx)
            {
                cx.emit(ThreadEvent::Error(error));
            }
            return;
        }
        if let Some(value) = trimmed.strip_prefix("budget ").map(str::trim) {
            let Some(goal) = self.goal() else {
                cx.emit(ThreadEvent::Error(anyhow::anyhow!("thread has no Goal")));
                return;
            };
            let budget = if matches!(value, "none" | "unlimited") {
                None
            } else {
                match value.parse::<u64>() {
                    Ok(value) => Some(value),
                    Err(error) => {
                        cx.emit(ThreadEvent::Error(error.into()));
                        return;
                    }
                }
            };
            if let Err(error) = self.edit_goal(goal.objective, budget, crate::db::GoalActor::User, cx)
            {
                cx.emit(ThreadEvent::Error(error));
            }
            return;
        }
        match trimmed.to_lowercase().as_str() {
            "" | "edit" | "replace" => {}
            "clear" => {
                if let Err(error) = self.clear_goal(crate::db::GoalActor::User, cx) {
                    cx.emit(ThreadEvent::Error(error));
                }
            }
            "pause" | "stop" => {
                if let Err(error) = self.set_goal_status(
                    crate::goal::GoalStatus::Paused,
                    Some("paused by user".into()),
                    crate::db::GoalActor::User,
                    cx,
                ) {
                    cx.emit(ThreadEvent::Error(error));
                }
            }
            "resume" => {
                if let Err(error) = self.set_goal_status(
                    crate::goal::GoalStatus::Active,
                    None,
                    crate::db::GoalActor::User,
                    cx,
                ) {
                    cx.emit(ThreadEvent::Error(error));
                }
            }
            _ => match self.set_goal(trimmed.to_string(), cx) {
                Ok(()) => self.insert_slash_turn(trimmed.to_string(), ui, cx),
                Err(error) => cx.emit(ThreadEvent::Error(error)),
            },
        }
    }

    /// Insert a user turn and run it, persisting the compact `/name args`
    /// display form — the shared tail of registry and built-in slash turns.
    fn insert_slash_turn(
        &mut self,
        text: String,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        let ordinal = self.user_prompt_ordinal();
        let display = ui.as_ref().and_then(|ui| ui.display_text.clone());
        self.insert_user_message_with_ui_metadata(text, ui, cx);
        self.persist_registry_display(ordinal, display);
        self.run_turn(cx);
    }

    /// The ordinal the next inserted user prompt will occupy among user-role
    /// prompt messages — the sidecar key for its compact display form. The
    /// count runs over the mirrored transcript (`self.messages`), which
    /// matches the pi session's `AgentMessage::User` entries: tool results
    /// and bash records are separate variants and never consume an ordinal.
    fn user_prompt_ordinal(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| {
                m.role == Role::User
                    && m.provenance == crate::message::MessageProvenance::User
            })
            .count()
    }

    /// Persist a registry turn's compact display form (`/key args`) in the
    /// session sidecar. The pi transcript stores only the expanded
    /// macro/skill body, so the sidecar is what lets `pi_engine::sync_history`
    /// restore the send-time bubble after a reload. Fire-and-forget: a lost
    /// write only narrows the reload window, the live bubble is unaffected.
    fn persist_registry_display(&self, ordinal: usize, display: Option<String>) {
        let Some(display) = display else {
            return;
        };
        let Some(sessions_dir) = crate::paths::manox_config_dir()
            .ok()
            .map(|dir| dir.join("pi-sessions"))
        else {
            return;
        };
        let Some(session_path) = self.active_session_path() else {
            return;
        };
        persist_registry_display_spawn(sessions_dir, session_path, ordinal, display);
    }

    /// Whether the pi backend restored an existing session at startup.
    pub fn restored(&self) -> bool {
        self.restored
    }

    /// The session file the backend currently drives. `None` while the engine
    /// is not materialized (a landing thread).
    pub fn active_session_path(&self) -> Option<PathBuf> {
        self.engine.as_ref().and_then(|e| e.active_session_path())
    }

    /// The sessions the backend can list (sidebar source), newest first.
    pub fn session_list(&self) -> Vec<crate::db::ThreadSummary> {
        self.engine
            .as_ref()
            .map(|e| e.session_list())
            .unwrap_or_default()
    }

    /// Re-point the backend at an existing session file.
    pub fn open_session(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.open_session(path);
        }
        self.running = false;
        cx.notify();
    }

    /// History-loading state (see `HistoryPhase`).
    pub fn history_phase(&self) -> HistoryPhase {
        self.history_phase
    }
}

fn persist_registry_display_spawn(
    sessions_dir: PathBuf,
    session_path: PathBuf,
    ordinal: usize,
    display: String,
) -> tokio::task::JoinHandle<()> {
    crate::runtime::handle().spawn(async move {
        let mut meta = pi_extensions::session_meta::load(&sessions_dir, &session_path)
            .await
            .unwrap_or_default();
        meta.registry_displays.insert(ordinal, display);
        if let Err(err) =
            pi_extensions::session_meta::save(&sessions_dir, &session_path, &meta).await
        {
            tracing::warn!(error = %err, "failed to persist registry display text");
        }
    })
}

impl Drop for Thread {
    fn drop(&mut self) {
        // The engine owns the actor; ask it to close gracefully. If the
        // channel is already gone the actor exited on its own. A landing
        // thread has no engine to shut down.
        if let Some(engine) = &self.engine {
            engine.shutdown();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A scripted engine for facade-contract tests: returns the injected
    /// authoritative history, records the mode/effort it was handed.
    pub(crate) struct FakeEngine {
        history: Vec<Message>,
        shutdown_calls: AtomicUsize,
        approval_mode: Mutex<Option<ApprovalMode>>,
        thinking_level: Mutex<Option<String>>,
        /// Recorded `run` calls: (prompt, images) pairs.
        runs: Mutex<Vec<(String, Vec<pi::types::ContentBlock>)>>,
        /// Recorded `persist_plan_snapshot` calls (serialized snapshots).
        plan_persists: Mutex<Vec<Option<serde_json::Value>>>,
    }

    impl FakeEngine {
        /// Empty-history engine for cross-module facade tests (team layer).
        pub(crate) fn new() -> Self {
            Self {
                history: Vec::new(),
                shutdown_calls: AtomicUsize::new(0),
                approval_mode: Mutex::new(None),
                thinking_level: Mutex::new(None),
                runs: Mutex::new(Vec::new()),
                plan_persists: Mutex::new(Vec::new()),
            }
        }
    }
    impl ThreadEngine for FakeEngine {
        fn persist_plan_snapshot(&self, snapshot: Option<serde_json::Value>) {
            self.plan_persists.lock().unwrap().push(snapshot);
        }

        fn is_running(&self) -> bool {
            false
        }

        fn history(&self) -> Vec<Message> {
            self.history.clone()
        }

        fn request_token_usage(&self) -> HashMap<String, TokenUsage> {
            HashMap::new()
        }

        fn model(&self) -> Option<PiModel> {
            None
        }

        fn run(&self, prompt: String, images: Vec<pi::types::ContentBlock>) {
            self.runs.lock().unwrap().push((prompt, images));
        }

        fn steer(&self, _text: String, _images: Vec<pi::types::ContentBlock>) -> String {
            String::new()
        }

        fn cancel_steer(&self, _id: &str) -> bool {
            false
        }

        fn abort(&self) {}

        fn set_model(&self, _model: PiModel) {}

        fn set_thinking_level(&self, level: Option<String>) {
            *self.thinking_level.lock().unwrap() = level;
        }

        fn open_session(&self, _path: PathBuf) {}

        fn new_session(&self, _cwd: PathBuf, _project: Option<PathBuf>) {}

        fn active_session_path(&self) -> Option<PathBuf> {
            None
        }

        fn session_list(&self) -> Vec<crate::db::ThreadSummary> {
            Vec::new()
        }

        fn shutdown(&self) {
            self.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn set_approval_mode(&self, mode: ApprovalMode) {
            *self.approval_mode.lock().unwrap() = Some(mode);
        }
    }

    /// Construct a `Thread` facade directly (no actor) with the given
    /// history-loading phase and engine, so the notice-contract tests below
    /// exercise `handle_notice` in isolation.
    pub(crate) fn thread_with_engine(
        phase: HistoryPhase,
        engine: Arc<dyn ThreadEngine>,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<Thread> {
        cx.update(|cx| {
            cx.new(|_| Thread {
                id: ThreadId("test-thread".to_string()),
                cwd: PathBuf::from("/tmp"),
                project: None,
                model: None,
                approval_mode: ApprovalMode::default(),
                messages: Vec::new(),
                reasoning_effort: ReasoningEffort::default(),
                pinned: false,
                archived: false,
                running: false,
                restored: false,
                ui_notes: Vec::new(),
                request_usage: HashMap::new(),
                pending_prompts: Vec::new(),
                pending_images: Vec::new(),
                pending_steers: VecDeque::new(),
                last_user_ui: None,
                engine: Some(engine),
                history_phase: phase,
                approval_mode_explicitly_set: false,
                plan_mode: false,
                persisted_plan: None,
                label: "lead".into(),
                team: None,
                goal_bridge: None,            worktree_path: None,
            })
        })
    }

    #[test]
    fn approval_mode_maps_i64_roundtrip() {
        assert_eq!(ApprovalMode::from_i64(0), ApprovalMode::AutoPilot);
        assert_eq!(ApprovalMode::from_i64(1), ApprovalMode::Danger);
        assert_eq!(ApprovalMode::from_i64(2), ApprovalMode::Danger);
        assert_eq!(ApprovalMode::AutoPilot.as_i64(), 0);
        assert_eq!(ApprovalMode::Danger.as_i64(), 1);
    }

    /// Registry turns originate on the GPUI main thread, outside Tokio's
    /// entered context. Persistence must still dispatch through Manox's
    /// process-global runtime instead of requiring a current reactor.
    #[gpui::test]
    fn registry_display_persistence_dispatches_off_runtime(cx: &mut gpui::TestAppContext) {
        cx.update(crate::runtime::init);

        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("session.jsonl");
        let sessions_dir = dir.path().to_path_buf();
        let display = "/gitwork:deliver fast".to_string();

        let result = std::panic::catch_unwind(|| {
            persist_registry_display_spawn(
                sessions_dir.clone(),
                session_path.clone(),
                0,
                display,
            )
        });
        let task = result.expect("dispatching from the GPUI thread must not require a reactor");
        crate::runtime::handle().block_on(task).unwrap();

        let meta = crate::runtime::handle()
            .block_on(pi_extensions::session_meta::load(
                &sessions_dir,
                &session_path,
            ))
            .unwrap();
        assert_eq!(
            meta.registry_displays.get(&0).map(String::as_str),
            Some("/gitwork:deliver fast")
        );
    }

    /// `LiveHistory` re-mirrors the engine's live transcript into `messages`
    /// (the switch-back rebuild reads the mirror) without emitting a UI
    /// event; an actor-initiated `TurnStarted` (monitor wakeup, plan-approval
    /// seed) flips the running flag so a switch-away parks the thread instead
    /// of dropping it mid-run, and `Settled` clears it.
    #[gpui::test]
    fn live_history_refreshes_mirror_and_actor_turn_started_sets_running(
        cx: &mut gpui::TestAppContext,
    ) {
        let engine = Arc::new(FakeEngine {
            history: vec![Message::assistant(vec![MessageContent::Text(
                "partial answer".into(),
            )])],
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine, cx);
        thread.update(cx, |t, cx| {
            // Mid-run mirror refresh: the live partial lands in `messages`.
            t.handle_notice(BackendNotice::LiveHistory, cx);
            assert_eq!(t.messages.len(), 1);
            assert!(matches!(
                &t.messages[0].content[0],
                MessageContent::Text(text) if text == "partial answer"
            ));
            assert!(!t.is_running());

            // Actor-initiated run start mirrors onto the facade running flag.
            t.handle_notice(BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)), cx);
            assert!(t.is_running());

            // Settlement releases the slot (and refreshes the mirror).
            t.handle_notice(
                BackendNotice::Settled {
                    cancelled: false,
                    failed: false,
                    steered: Vec::new(),
                    stranded: Vec::new(),
                },
                cx,
            );
            assert!(!t.is_running());
        });
    }

    /// The headless slash router drives the same thread state the gpui host
    /// toggles: plan mode, approval mode, compact, and goal lifecycle.
    #[gpui::test]
    fn run_slash_builtin_plan_danger_compact_and_unowned(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone(), cx);
        thread.update(cx, |t, cx| {
            // The prompt form enters plan mode and runs the turn with the
            // compact display form.
            let ui = MessageUiMetadata {
                display_text: Some("/plan fix it".into()),
                ..Default::default()
            };
            assert!(t.run_slash_builtin("plan", "fix it", Some(ui.clone()), cx));
            assert!(t.plan_mode(), "/plan <prompt> enters plan mode");
            let runs = engine.runs.lock().unwrap();
            assert_eq!(runs.len(), 1, "prompt form runs a turn");
            assert_eq!(runs[0].0, "fix it");
            drop(runs);
            let last = t.messages().last().expect("turn message inserted");
            assert_eq!(
                last.ui.as_ref().and_then(|ui| ui.display_text.as_deref()),
                Some("/plan fix it")
            );

            // A second invocation toggles plan mode back off, no new turn.
            assert!(t.run_slash_builtin("plan", "", None, cx));
            assert!(!t.plan_mode(), "/plan bare exits plan mode");
            assert_eq!(engine.runs.lock().unwrap().len(), 1);

            assert!(t.run_slash_builtin("danger", "", None, cx));
            assert_eq!(t.approval_mode(), ApprovalMode::Danger);

            assert!(t.run_slash_builtin("compact", "focus", None, cx));
            assert!(t.run_slash_builtin("goal", "clear", None, cx));

            // Session-level commands and unknowns are not owned here.
            assert!(!t.run_slash_builtin("exit", "", None, cx));
            assert!(!t.run_slash_builtin("quit", "", None, cx));
            assert!(!t.run_slash_builtin("new", "", None, cx));
            assert!(!t.run_slash_builtin("nope", "", None, cx));
        });
    }

    #[gpui::test]
    fn live_history_reattaches_ui_metadata_to_prompt_not_tool_result(
        cx: &mut gpui::TestAppContext,
    ) {
        let engine = Arc::new(FakeEngine {
            history: vec![
                Message::user("expanded registry prompt".to_string()),
                Message::user_with_content(vec![MessageContent::ToolResult(
                    crate::language_model::LanguageModelToolResult {
                        tool_use_id: "tu_1".into(),
                        tool_name: "Read".into(),
                        is_error: false,
                        content: "done".into(),
                    },
                )]),
            ],
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine, cx);
        thread.update(cx, |t, cx| {
            t.insert_user_message_with_ui_metadata(
                "expanded registry prompt".to_string(),
                Some(MessageUiMetadata {
                    display_text: Some("/gitwork:deliver fast".to_string()),
                    ..Default::default()
                }),
                cx,
            );
            t.handle_notice(BackendNotice::LiveHistory, cx);

            assert_eq!(
                t.messages[0]
                    .ui
                    .as_ref()
                    .and_then(|ui| ui.display_text.as_deref()),
                Some("/gitwork:deliver fast")
            );
            assert!(
                t.messages[1].ui.is_none(),
                "tool-result user messages must not inherit prompt UI metadata"
            );
        });
    }

    fn png_image(data: &str) -> MessageContent {
        MessageContent::Image {
            data: data.to_string(),
            mime_type: "image/png".to_string(),
        }
    }

    /// TS parity: images ride the prompt's own user message. `run_turn` must
    /// hand the queued text AND the queued images to the engine in one turn,
    /// draining both queues.
    #[gpui::test]
    fn run_turn_drains_pending_text_and_images(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone(), cx);
        thread.update(cx, |t, cx| {
            t.insert_user_message_with_content_and_ui_metadata(
                vec![
                    MessageContent::Text("look at these".to_string()),
                    png_image("aW1hZ2Ux"),
                    png_image("aW1hZ2Uy"),
                ],
                None,
                cx,
            );
            t.run_turn(cx);
        });
        let runs = engine.runs.lock().unwrap();
        assert_eq!(runs.len(), 1, "exactly one turn ran");
        let (prompt, images) = &runs[0];
        assert_eq!(prompt, "look at these");
        assert_eq!(images.len(), 2, "both images ride the turn");
        drop(runs);
        thread.update(cx, |t, _| {
            assert!(t.pending_prompts.is_empty(), "text queue drained");
            assert!(t.pending_images.is_empty(), "image queue drained");
        });
    }

    /// An image-only insert (no text) still starts a turn — the guard keys on
    /// BOTH queues being empty, so the engine receives an empty prompt plus
    /// the image (kernel pushes the empty text block, TS parity).
    #[gpui::test]
    fn run_turn_image_only_insert_still_starts_turn(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone(), cx);
        thread.update(cx, |t, cx| {
            t.insert_user_message_with_content_and_ui_metadata(
                vec![png_image("aW1hZ2Ux")],
                None,
                cx,
            );
            t.run_turn(cx);
        });
        let runs = engine.runs.lock().unwrap();
        assert_eq!(runs.len(), 1, "image-only turn ran");
        let (prompt, images) = &runs[0];
        assert_eq!(prompt, "", "no text was queued");
        assert_eq!(images.len(), 1);
    }

    /// With neither text nor images pending, `run_turn` is a no-op.
    #[gpui::test]
    fn run_turn_noop_when_nothing_pending(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone(), cx);
        thread.update(cx, |t, cx| t.run_turn(cx));
        assert!(engine.runs.lock().unwrap().is_empty(), "no turn ran");
    }

    /// The race-fix contract: `Ready` — regardless of preview batches that
    /// may have streamed before it — replaces the mirror with the engine's
    /// authoritative history and clears `Loading`, so the workspace leaves
    /// the spinner and re-enables input.
    #[gpui::test]
    fn ready_replaces_preview_with_authoritative_history_and_clears_loading(
        cx: &mut gpui::TestAppContext,
    ) {
        let engine = Arc::new(FakeEngine {
            history: vec![Message::user("authoritative".to_string())],
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Loading, engine, cx);
        // Simulate a preview batch that landed before the authoritative sync.
        thread.update(cx, |t, cx| {
            t.messages = vec![Message::user("preview-only".to_string())];
            t.handle_notice(
                BackendNotice::Ready {
                    restored: true,
                    model: None,
                    approval_mode: ApprovalMode::default(),
                    plan_mode: false,
                    plan_file: None,
                    plan_review_pending: false,
                    plan_snapshot: None,
                },
                cx,
            );
        });
        let (phase, texts) = cx.read(|cx| {
            let t = thread.read(cx);
            let texts: Vec<String> = t
                .messages()
                .iter()
                .filter_map(|m| match &m.content[0] {
                    MessageContent::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            (t.history_phase(), texts)
        });
        assert_eq!(phase, HistoryPhase::Ready);
        assert_eq!(
            texts,
            vec!["authoritative".to_string()],
            "the authoritative sync wins over any preview content"
        );
    }

    /// K1: a `Fatal` before `Ready` (no model configured, session build
    /// failed — the actor bails without `Ready`) must clear `Loading` and
    /// drop any stale preview, so the workspace returns to the hero instead
    /// of spinning forever with input gated.
    #[gpui::test]
    fn fatal_before_ready_clears_loading_and_preview(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Loading, engine, cx);
        thread.update(cx, |t, cx| {
            t.messages = vec![Message::user("stale-preview".to_string())];
            t.handle_notice(BackendNotice::Fatal(anyhow::anyhow!("no model configured")), cx);
        });
        let (phase, count) = cx.read(|cx| {
            let t = thread.read(cx);
            (t.history_phase(), t.messages().len())
        });
        assert_eq!(phase, HistoryPhase::Ready, "input gate opens");
        assert_eq!(count, 0, "stale preview is dropped");
    }

    /// I3: an explicit user approval-mode choice on a landing thread survives
    /// the sidecar default arriving at `Ready` (`approval_mode_explicitly_set`).
    #[gpui::test]
    fn explicit_approval_mode_survives_ready_sidecar_default(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine, cx);
        thread.update(cx, |t, _cx| {
            t.set_approval_mode(ApprovalMode::Danger, _cx);
        });
        // The fresh session's sidecar reports AutoPilot at Ready; the user's
        // Danger choice must not be overwritten.
        thread.update(cx, |t, cx| {
            t.handle_notice(
                BackendNotice::Ready {
                    restored: false,
                    model: None,
                    approval_mode: ApprovalMode::AutoPilot,
                    plan_mode: false,
                    plan_file: None,
                    plan_review_pending: false,
                    plan_snapshot: None,
                },
                cx,
            );
        });
        assert_eq!(cx.read(|cx| thread.read(cx).approval_mode()), ApprovalMode::Danger);
    }

    /// P1: `PlanUpdated` mirrors onto the facade and persists through the
    /// engine; an empty snapshot clears both (the model dropped its plan).
    #[gpui::test]
    fn plan_updated_mirrors_and_persists(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let engine_ref = Arc::clone(&engine);
        let thread = thread_with_engine(HistoryPhase::Ready, engine, cx);

        let snapshot = crate::plan::PlanSnapshot {
            explanation: None,
            steps: vec![crate::plan::PlanStep {
                step: "investigate".to_string(),
                status: crate::plan::PlanStepStatus::InProgress,
            }],
        };
        thread.update(cx, |t, cx| {
            t.handle_notice(
                BackendNotice::Event(Box::new(crate::thread::ThreadEvent::PlanUpdated {
                    snapshot: snapshot.clone(),
                })),
                cx,
            );
        });
        cx.read(|cx| {
            assert_eq!(thread.read(cx).persisted_plan(), Some(&snapshot));
        });
        let persists = engine_ref.plan_persists.lock().unwrap();
        assert_eq!(persists.len(), 1);
        let stored: crate::plan::PlanSnapshot =
            serde_json::from_value(persists[0].clone().unwrap()).unwrap();
        assert_eq!(stored, snapshot);
        drop(persists);

        // Empty snapshot = the model cleared its plan → mirror + sidecar clear.
        thread.update(cx, |t, cx| {
            t.handle_notice(
                BackendNotice::Event(Box::new(crate::thread::ThreadEvent::PlanUpdated {
                    snapshot: crate::plan::PlanSnapshot {
                        explanation: None,
                        steps: Vec::new(),
                    },
                })),
                cx,
            );
        });
        cx.read(|cx| {
            assert_eq!(thread.read(cx).persisted_plan(), None);
        });
        let persists = engine_ref.plan_persists.lock().unwrap();
        assert_eq!(persists.len(), 2);
        assert!(persists[1].is_none());
    }

    /// P2: `Ready` restores the persisted plan snapshot (post-restart /
    /// thread-switch source for the rail's fallback).
    #[gpui::test]
    fn ready_restores_persisted_plan_snapshot(cx: &mut gpui::TestAppContext) {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            approval_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Loading, engine, cx);
        let snapshot = crate::plan::PlanSnapshot {
            explanation: None,
            steps: vec![crate::plan::PlanStep {
                step: "implement".to_string(),
                status: crate::plan::PlanStepStatus::Completed,
            }],
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        thread.update(cx, |t, cx| {
            t.handle_notice(
                BackendNotice::Ready {
                    restored: true,
                    model: None,
                    approval_mode: ApprovalMode::default(),
                    plan_mode: false,
                    plan_file: None,
                    plan_review_pending: false,
                    plan_snapshot: Some(value),
                },
                cx,
            );
        });
        cx.read(|cx| {
            assert_eq!(thread.read(cx).persisted_plan(), Some(&snapshot));
        });
    }
}
