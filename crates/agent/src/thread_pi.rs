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
use crate::language::Language;
use crate::language_model::{MessageContent, ReasoningEffort, StopReason, TokenUsage};
use pi::types::Model as PiModel;
use crate::message::{Message, MessageUiMetadata};
use crate::thread_engine::{BackendNotice, SpawnedEngine, ThreadEngine};

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

/// Events emitted by `Thread` to the UI. The pi backend produces the run
/// lifecycle subset; the remaining variants exist so the workspace and
/// conversation list compile against the shared contract and simply never
/// fire.
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
    /// Live `<proposed_plan>` block delta. Plan mode is a manox flow; the pi
    /// backend never emits it.
    PlanDelta { delta: String },
    /// A turn ended with a complete `<proposed_plan>` block.
    PlanReady { plan_text: String },
    /// The model published a structured task list.
    PlanUpdated {
        snapshot: crate::plan::PlanSnapshot,
    },
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
    /// Auto-compaction summarization pass started.
    CompactionStarted { tokens_before: u64 },
    /// A compaction pass landed.
    Compaction {
        summary: String,
        messages_compacted: usize,
        tokens_before: u64,
    },
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
    /// Steer message ids handed to the engine this run, awaiting settlement.
    pending_steers: VecDeque<String>,
    /// UI metadata of the most recently inserted user turn, re-attached to
    /// the authoritative history's last user message after each refresh.
    last_user_ui: Option<MessageUiMetadata>,
    engine: Arc<dyn ThreadEngine>,
}

impl EventEmitter<ThreadEvent> for Thread {}

impl Thread {
    /// Startup constructor: restores the newest session when one exists.
    pub fn new(id: ThreadId, cwd: PathBuf, cx: &mut App) -> Entity<Self> {
        Self::open(id, cwd, None, None, false, cx)
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
        let model = crate::pi_providers::default_model();
        let sessions_dir = crate::paths::manox_config_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("pi-sessions");
        let SpawnedEngine { engine, mut events } = crate::pi_engine::spawn_engine(
            cwd.clone(),
            model.clone(),
            sessions_dir.clone(),
            initial_path,
            fresh,
            project.clone(),
        );

        cx.new(|cx| {
            // The gpui drainer: adapts backend notices onto this entity and
            // refreshes the mirrored history/usage on settlement.
            cx.spawn(async move |this, cx| {
                while let Some(notice) = events.recv().await {
                    let ok = this
                        .update(cx, |t: &mut Thread, cx| match notice {
                            BackendNotice::Event(event) => {
                                // Mirror the gate policy before the chip
                                // hears about the change.
                                if let ThreadEvent::ApprovalModeChanged { mode } = *event {
                                    t.approval_mode = mode;
                                }
                                cx.emit(*event);
                            }
                            BackendNotice::Ready {
                                restored,
                                model,
                                approval_mode,
                            } => {
                                t.restored = restored;
                                if let Some(m) = model {
                                    t.model = Some(m);
                                }
                                t.approval_mode = approval_mode;
                                if restored {
                                    t.refresh_history(cx);
                                    cx.emit(ThreadEvent::HistoryRestored);
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
                                t.running = false;
                                t.pending_steers.clear();
                                t.refresh_history(cx);
                                cx.emit(ThreadEvent::TurnFinished {
                                    cancelled,
                                    failed,
                                    stranded_steer_ids: stranded,
                                });
                            }
                            BackendNotice::Fatal(err) => {
                                t.running = false;
                                cx.emit(ThreadEvent::Error(err));
                            }
                            BackendNotice::SessionListDirty => {
                                let store = crate::thread_store::global();
                                store.update(cx, |s, cx| s.refresh(cx));
                            }
                        })
                        .is_ok();
                    if !ok {
                        break;
                    }
                }
            })
            .detach();

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
                pending_steers: VecDeque::new(),
                last_user_ui: None,
                engine,
            }
        })
    }

    /// Restore the bound project from a reopened session's sidecar without
    /// recreating the session (used by the store on load).
    pub fn restore_project(&mut self, dir: PathBuf) {
        self.cwd = dir.clone();
        self.project = Some(dir);
    }

    /// Replace the mirrored history with the engine's authoritative transcript
    /// and re-attach the last user turn's UI metadata.
    fn refresh_history(&mut self, cx: &mut Context<Self>) {
        let mut mapped = self.engine.history();
        if let Some(ui) = self.last_user_ui.clone()
            && let Some(last_user) = mapped
                .iter_mut()
                .rev()
                .find(|m| matches!(m.role, crate::language_model::Role::User))
        {
            last_user.ui = Some(ui);
        }
        self.messages = mapped;
        self.request_usage = self.engine.request_token_usage();
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
        // Image attachments are not wired yet — pi prompts are text-only in
        // this stage; image blocks are dropped from the prompt text.
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
        self.pending_steers.push_back(id.clone());
        self.engine.steer(text);
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
        self.engine.run(prompt);
        cx.notify();
    }

    pub fn cancel(&mut self, _cx: &mut Context<Self>) {
        self.engine.abort();
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
        self.engine.cumulative_token_usage()
    }

    pub fn per_model_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.engine.per_model_token_usage()
    }

    pub fn ui_notes(&self) -> &[UiNoteRecord] {
        &self.ui_notes
    }

    pub fn worktree(&self) -> Option<&WorktreeState> {
        None
    }

    pub fn goal(&self) -> Option<&ThreadGoal> {
        None
    }

    pub fn depth(&self) -> u32 {
        0
    }

    pub fn agent_label(&self) -> &str {
        "lead"
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
    pub fn seed_approved_plan(
        &mut self,
        plan_text: String,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        let text = crate::collaboration_mode::implement_plan_user_message(&plan_text);
        self.insert_user_message_with_ui_metadata(text, ui, cx);
    }

    /// Seed the approved plan and run the implementation turn on this
    /// thread (the non-clear `Implement` verdict).
    pub fn implement_approved_plan(
        &mut self,
        plan_text: String,
        ui: Option<MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        self.seed_approved_plan(plan_text, ui, cx);
        self.run_turn(cx);
    }

    pub fn set_project(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.has_interacted() {
            return;
        }
        self.cwd = dir.clone();
        self.project = Some(dir.clone());
        self.engine.new_session(dir.clone(), Some(dir));
        cx.notify();
    }

    /// Manual compaction (`/compact`): no-op while a turn is in flight (the
    /// kernel compacts an idle transcript only); the recap card lands via the
    /// engine's harness-event adaptation.
    pub fn compact(&mut self, custom_instructions: Option<String>, _cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.engine.compact(custom_instructions);
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        if self.approval_mode == mode {
            return;
        }
        self.approval_mode = mode;
        // The engine applies the mode to its gate and persists it in the
        // session sidecar; the chip reflects the change immediately.
        self.engine.set_approval_mode(mode);
        cx.emit(ThreadEvent::ApprovalModeChanged { mode });
        cx.notify();
    }

    /// Deliver the user's verdict for a pending tool-call authorization
    /// (approval card or `AskUserQuestion`). Unknown ids are ignored.
    pub fn respond_authorization(
        &mut self,
        id: &str,
        response: crate::permission::ToolAuthorizationResponse,
        _cx: &mut Context<Self>,
    ) {
        self.engine.respond_tool_authorization(id, response);
    }

    /// Pending authorizations with their card metadata, so the workspace can
    /// re-surface a card after switching back to this thread.
    pub fn pending_auth_entries(&self) -> Vec<(String, crate::permission::PendingAuthMeta)> {
        self.engine.pending_auth_entries()
    }

    pub fn set_pinned(&mut self, pinned: bool, cx: &mut Context<Self>) {
        self.pinned = pinned;
        cx.notify();
    }


    pub fn set_model(&mut self, model: PiModel, cx: &mut Context<Self>) {
        let from = self.model.as_ref().map(|m| m.id.clone());
        let to = model.id.clone();
        self.model = Some(model.clone());
        self.engine.set_model(model);
        cx.emit(ThreadEvent::ModelChanged { from, to });
        cx.notify();
    }

    pub fn set_reasoning_effort(&mut self, effort: ReasoningEffort, cx: &mut Context<Self>) {
        if self.reasoning_effort == effort {
            return;
        }
        self.reasoning_effort = effort;
        self.engine.set_thinking_level(Some(effort.wire_value().to_string()));
        cx.emit(ThreadEvent::ReasoningEffortChanged { effort });
        cx.notify();
    }

    pub fn set_archived(&mut self, archived: bool, cx: &mut Context<Self>) {
        self.archived = archived;
        cx.notify();
    }
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
            self.engine.cancel_steer(id);
            true
        } else {
            false
        }
    }

    pub fn push_ui_note(&mut self, note: UiNoteRecord) {
        self.ui_notes.push(note);
    }

    pub fn submit_command(&mut self, _name: &str, _args: &str, _cx: &mut Context<Self>) -> bool {
        // Slash commands are a manox registry feature.
        false
    }

    pub fn submit_skill(&mut self, _key: &str, _args: &str, _cx: &mut Context<Self>) -> bool {
        // Skills are a manox registry feature.
        false
    }

    /// Whether the pi backend restored an existing session at startup.
    pub fn restored(&self) -> bool {
        self.restored
    }

    /// The session file the backend currently drives.
    pub fn active_session_path(&self) -> Option<PathBuf> {
        self.engine.active_session_path()
    }

    /// The sessions the backend can list (sidebar source), newest first.
    pub fn session_list(&self) -> Vec<crate::db::ThreadSummary> {
        self.engine.session_list()
    }

    /// Re-point the backend at an existing session file.
    pub fn open_session(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.engine.open_session(path);
        self.running = false;
        cx.notify();
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        // The engine owns the actor; ask it to close gracefully. If the
        // channel is already gone the actor exited on its own.
        self.engine.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_mode_maps_i64_roundtrip() {
        assert_eq!(ApprovalMode::from_i64(0), ApprovalMode::AutoPilot);
        assert_eq!(ApprovalMode::from_i64(1), ApprovalMode::Danger);
        assert_eq!(ApprovalMode::from_i64(2), ApprovalMode::Danger);
        assert_eq!(ApprovalMode::AutoPilot.as_i64(), 0);
        assert_eq!(ApprovalMode::Danger.as_i64(), 1);
    }
}
