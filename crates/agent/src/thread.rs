//! `Thread` state machine.
//!
//! gpui-native: `Thread` is an `Entity<Thread>` + `EventEmitter<ThreadEvent>`.
//! `run_turn` spawns a task on the gpui executor that loops:
//!   1. `build_completion_request` → `model.stream_completion` yields a `BoxStream`;
//!   2. drain events via `handle_completion_event`, collecting `pending_tool_uses`;
//!   3. after the stream ends, if tool_uses were collected: authorize (if needed) → `tool.run` → append a ToolResult message → loop back to 1;
//!   4. otherwise (EndTurn) exit.
//!
//! Authorization uses a `tokio::sync::oneshot`: `Thread` emits `ToolCallAuthorization`
//! carrying a `Sender`; the UI resolves the prompt and sends back a
//! `ToolAuthorizationResponse` (a permission decision or, for `AskUserQuestion`,
//! the user's answers), which the task `await`s on the gpui executor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use futures::FutureExt as _;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::db::ThreadRecord;
use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, LanguageModelToolResult, LanguageModelToolUse, MessageContent,
    Role, StopReason, TokenUsage,
};
use crate::message::Message;
use crate::prefix_stability::StablePrefix;
use crate::title_state::TitleState;
use crate::token_meter::TokenMeter;
use crate::tool::{
    PermissionCache, PermissionDecision, PlanApprovalResponse, ToolAuthorizationResponse,
    ToolRegistry, exit_plan_mode_request_tool,
};
use crate::tools;

/// Stable `Thread` id used for persistence.
#[derive(Debug, Clone)]
pub struct ThreadId(pub String);

/// Tool call status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    PendingApproval,
    Running,
    Success,
    Error,
    Denied,
}

/// User-facing approval policy for tool calls. Drives both the access chip in
/// the composer and the free / approval branch in `run_turn_loop`. Persisted on
/// the thread so switching threads restores the mode the user last picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Every approval-required tool call shows the authorization overlay.
    #[serde(rename = "on-request")]
    #[default]
    OnRequest,
    /// A built-in security-reviewer LLM agent vet each approval-required tool
    /// call. Safe calls run without prompting; risky ones still raise the
    /// overlay (with a one-line reason from the agent). Failures (timeout,
    /// parse error, unsupported verdict) fall back to the overlay.
    #[serde(rename = "auto-review")]
    AutoReview,
    /// All approvals are bypassed and `bash` runs unsandboxed
    /// (DangerFullAccess equivalent). Inherited by sub-agents.
    #[serde(rename = "yolo")]
    Yolo,
}

impl ApprovalMode {
    pub fn from_i64(v: i64) -> Self {
        match v {
            1 => Self::AutoReview,
            2 => Self::Yolo,
            _ => Self::OnRequest,
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Self::OnRequest => 0,
            Self::AutoReview => 1,
            Self::Yolo => 2,
        }
    }
}

/// Events emitted by `Thread` to the UI.
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
    },
    /// Tool execution result (output fed back to the model and shown in the UI).
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
    /// Live output chunk from a streaming tool (e.g. `bash` stdout/stderr).
    /// Accumulated into the matching tool-call item's `output` until the
    /// final `ToolResult` overwrites it with the canonical (truncated) text.
    ToolOutput { id: String, chunk: String },
    /// Request user authorization for a tool call. The UI resolves the prompt and calls `Thread::respond_authorization` with a `ToolAuthorizationResponse` (a decision, or — for `AskUserQuestion` — the user's answers).
    ToolCallAuthorization {
        id: String,
        tool_name: String,
        summary: String,
        input: serde_json::Value,
    },
    /// Approval mode changed. The UI refreshes its badge, access chip, and
    /// the popover's selected row.
    ApprovalModeChanged { mode: ApprovalMode },
    /// A completion turn started. Signals the UI (via `ThreadStore`) that this
    /// thread is now running — covers the gap before the first `AgentText`/
    /// `AgentThinking` delta arrives (model warming up, network latency) so
    /// the running indicator is immediate. Terminal `Stop`/`Error` clear it.
    TurnStarted,
    /// A completion turn ended.
    Stop(StopReason),
    /// An error during streaming.
    Error(anyhow::Error),
    /// The model called `exit_plan_mode` and submitted a plan. The UI shows an
    /// approval overlay; resolution arrives via `respond_plan_approval`.
    PlanProposed { id: String, plan_text: String },
    /// The prefix-stability fingerprint for this turn vs. the previous one.
    /// Emitted every turn with the current stability ratio plus the drift
    /// flags so subscribers (e.g. future telemetry or debug views) can
    /// observe cache discipline without scraping internal state. The
    /// composer chip that used to render this was removed in #62; the
    /// event is still emitted for forward-compat subscribers.
    PrefixStability {
        stability_pct: u16,
        system_changed: bool,
        tools_changed: bool,
    },
    /// Cumulative token usage changed (a `UsageUpdate` landed). The UI refreshes
    /// its token counter. Carries the thread-wide cumulative, not per-request.
    TokenUsageUpdated(TokenUsage),
    /// The user switched models mid-conversation. The store records a
    /// `model_change` event for the history timeline; emitted from `set_model`.
    ModelChanged { from: Option<String>, to: String },
}

/// UI metadata for a pending tool-call authorization. Stored alongside the
/// oneshot sender so the workspace can re-emit `ToolCallAuthorization` when
/// the user switches back to a thread parked on an approval prompt.
pub struct PendingAuthMeta {
    pub tool_name: String,
    pub summary: String,
    pub input: serde_json::Value,
}

pub struct Thread {
    pub id: ThreadId,
    messages: Vec<Message>,
    model: Option<AnyLanguageModel>,
    tools: Arc<ToolRegistry>,
    permission: Arc<PermissionCache>,
    /// Approval mode for this thread. Drives the access chip and the
    /// free / approval branch in `run_turn_loop`. Thread-scoped; inherited by
    /// sub-agents. Distinct from the always-allow cache so flipping modes
    /// returns to normal approval without leaving stale always-allow grants.
    approval_mode: ApprovalMode,
    /// Reasons the auto-review approval agent attached to `Ask` verdicts, keyed
    /// by `tool_use.id`. Drained into the auth overlay as a one-line
    /// justification. Cleared every turn with the rest of the per-turn state.
    approval_ask_reasons: HashMap<String, String>,
    cwd: PathBuf,
    /// The project directory the thread is bound to, chosen on the first screen.
    /// `None` means no project was chosen; tools then resolve paths against the
    /// app launch directory (`cwd`). Once set, it is fixed for the thread's life.
    project: Option<PathBuf>,
    /// tool_uses collected during the current turn, processed after the stream ends.
    pending_tool_uses: Vec<LanguageModelToolUse>,
    /// Pending authorizations for THIS thread's own tool calls, keyed by
    /// tool_use id. A map (not a single slot) so parallel tool calls can each
    /// await their own decision without overwriting one another.
    pub(crate) pending_authorizations:
        HashMap<String, tokio::sync::oneshot::Sender<ToolAuthorizationResponse>>,
    /// UI metadata for pending authorizations, mirroring `pending_authorizations`.
    /// Stored so the workspace can re-emit `ToolCallAuthorization` events when
    /// switching back to a thread that was parked waiting for user approval.
    pending_auth_meta: HashMap<String, PendingAuthMeta>,
    /// Authorization requests bubbled up from a sub-agent, keyed by a composite
    /// id `<parent_tool_use_id>::<child_auth_id>`. Routing a response back
    /// forwards it to the owning child thread.
    pending_child_auth: HashMap<String, ChildAuthRoute>,
    /// Sub-agent system prompt; `None` for the main thread (no system prompt injected).
    system: Option<String>,
    /// Nesting depth. Main thread = 0; a sub-agent = parent depth + 1.
    depth: u32,
    /// Max agentic turns before a sub-agent is force-stopped. `None` = unlimited.
    max_turns: Option<u32>,
    /// Completed round-trips in the current turn, for `max_turns` enforcement.
    turn_count: u32,
    /// Whether the max-turns summary turn has already been injected. The cap is
    /// allowed one extra round-trip so the sub-agent can produce a coherent
    /// final message instead of ending mid-work; a second cap hit hard-stops.
    cap_summary_injected: bool,
    /// The running turn task; dropping it aborts the turn.
    running_turn: Option<Task<()>>,
    /// Cancellation token for the running turn. Cancelled by `cancel()` so the
    /// in-flight tool (e.g. `bash`) can reap its process group promptly instead
    /// of relying on task-drop, which does not reach the detached tokio child.
    turn_cancel: Option<CancellationToken>,
    /// Optional tool whitelist for the current turn, set by a slash command's
    /// `allowed-tools` frontmatter. `None` or empty = inherit all tools. The
    /// filter lasts for the turn's lifetime and is cleared when the turn ends,
    /// so it never leaks into a subsequent free-form message.
    turn_tool_filter: Option<Vec<String>>,
    /// Whether the SessionStart hook has fired for this thread. Main threads
    /// (depth 0) fire once on their first turn; sub-agents never fire it.
    session_started: bool,
    /// Whether this thread is in plan mode. When true, `build_completion_request`
    /// filters the tool list to read-only tools plus `exit_plan_mode` and
    /// appends the plan-mode system-prompt addendum; `run_tool_inner` intercepts
    /// `exit_plan_mode` to run the approval handshake. Main-thread only —
    /// sub-agents always have `plan_mode == false`.
    plan_mode: bool,
    /// Pending plan-approval oneshots, keyed by the `exit_plan_mode` tool_use
    /// id. Mirrors `pending_authorizations` exactly.
    pending_plan_approval: HashMap<String, tokio::sync::oneshot::Sender<PlanApprovalResponse>>,
    /// LLM title + re-eval cadence + user rename. The spawn that drives a
    /// title turn lives in [`TitleState::maybe_generate`]; `Thread` passes in
    /// depth / model / messages each call. `pub(crate)` so the spawn callback
    /// in `title_state.rs` can write back the in-flight lock and the new title.
    pub(crate) title_state: TitleState,
    /// Fingerprint of the system prompt + tool specs, tracked turn-over-turn
    /// so prefix drift (a history rewrite, tool hot-reload, plan-mode toggle)
    /// is observable rather than silently busting the provider's prefix cache.
    /// See [`crate::prefix_stability`].
    prefix_stability: StablePrefix,
    /// Provider id of the current model, for per-provider stats in the sidebar.
    provider_id: Option<String>,
    /// Parent thread id for sub-agent lineage. `None` for main threads.
    parent_id: Option<String>,
    /// Whether the user archived this thread from the sidebar.
    archived: bool,
    /// Whether the user pinned this thread from the title bar menu. Pinned
    /// threads float to the top of the sidebar list. Persisted in the same
    /// row as `archived` so the two boolean metadata flags stay co-located.
    pinned: bool,
    /// Creation time (Unix seconds). Stable for the thread's life; persisted so
    /// the sidebar can show "created" separately from "last active".
    created_at: i64,
    /// Last real user interaction (Unix seconds). Advances on message submit;
    /// distinct from `updated_at` (any write) so background saves don't reset
    /// the "last active" ordering in the sidebar.
    interacted_at: i64,
    /// Streaming token-usage accounting. Cumulative across the thread's life,
    /// plus per-user-message attribution persisted to the `token_usage` table.
    /// Decoupled from the message list: the triggering user-message id is
    /// passed in by `Thread` on each `accumulate` / `finalize_request`.
    token_meter: TokenMeter,
    /// Monotonic counter stamped into every `snapshot()` so the db `upsert`
    /// can reject out-of-order writes. `save_thread` is fire-and-forget: an
    /// older snapshot (e.g. taken at submit, before the turn produced assistant
    /// content) can commit after a newer one if the background executor reorders
    /// them. The revision lets `upsert` refuse the stale write instead of
    /// clobbering the newer state — see `ThreadsDatabase::upsert`.
    persist_revision: AtomicU64,
}

/// A forwarded authorization request from a sub-agent: which child thread holds
/// the pending decision and what id the child knows it by.
struct ChildAuthRoute {
    child: WeakEntity<Thread>,
    child_auth_id: String,
}

impl EventEmitter<ThreadEvent> for Thread {}

impl Thread {
    /// Construct a new `Thread`, defaulting to the registry's first model and registering the built-in tools plus the `agent` tool.
    pub fn new(id: ThreadId, cwd: PathBuf, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let weak = cx.weak_entity();
            let model = crate::provider::registry::global()
                .models()
                .first()
                .cloned();
            let provider_id = model.as_ref().map(|m| m.provider_id());
            let now = chrono::Utc::now().timestamp();
            Self {
                id,
                messages: Vec::new(),
                model,
                tools: Arc::new(tools::main_registry(cwd.clone(), weak)),
                permission: Arc::new(PermissionCache::default()),
                approval_mode: ApprovalMode::default(),
                approval_ask_reasons: HashMap::new(),
                cwd,
                project: None,
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
                pending_auth_meta: HashMap::new(),
                pending_child_auth: HashMap::new(),
                system: None,
                depth: 0,
                max_turns: None,
                turn_count: 0,
                cap_summary_injected: false,
                running_turn: None,
                turn_cancel: None,
                turn_tool_filter: None,
                session_started: false,
                plan_mode: false,
                pending_plan_approval: HashMap::new(),
                title_state: TitleState::default(),
                prefix_stability: StablePrefix::default(),
                provider_id,
                parent_id: None,
                archived: false,
                pinned: false,
                created_at: now,
                interacted_at: now,
                token_meter: TokenMeter::default(),
                persist_revision: AtomicU64::new(0),
            }
        })
    }

    /// Restore a `Thread` from a persisted record (messages + model rebuilt; tools rebuilt from cwd).
    pub fn restore(
        rec: ThreadRecord,
        model: Option<AnyLanguageModel>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            // Continue the title cadence from where the persisted thread left
            // off: a thread that already has a title is treated as if it had
            // been evaluated at the current user-count, so it does not re-run
            // the title stream on its first post-reload turn.
            //
            // Count only user messages with text (real user turns); tool
            // results are role User but carry no user-typed text, so including
            // them would inflate the count and shift the cadence in tool-heavy
            // turns. Mirrors the filter in `maybe_generate_title`.
            let user_count = rec
                .messages
                .iter()
                .filter(|m| m.role == Role::User && message_has_text(m))
                .count();
            let title_last_eval_user_count = rec.title.as_ref().map(|_| user_count);
            let weak = cx.weak_entity();
            let cwd = std::path::PathBuf::from(&rec.cwd);
            let project = (!rec.project.is_empty()).then(|| std::path::PathBuf::from(&rec.project));
            // Persisted provider_id wins; a thread saved before its model was
            // resolved (provider_id stays None until set_model) picks up the
            // restore-time resolved model's provider.
            let provider_id = rec
                .provider_id
                .clone()
                .or_else(|| model.as_ref().map(|m| m.provider_id()));
            Self {
                id: ThreadId(rec.id),
                messages: rec.messages,
                model,
                tools: Arc::new(tools::main_registry(cwd.clone(), weak)),
                permission: Arc::new(PermissionCache::default()),
                approval_mode: ApprovalMode::from_i64(rec.approval_mode),
                approval_ask_reasons: HashMap::new(),
                cwd,
                project,
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
                pending_auth_meta: HashMap::new(),
                pending_child_auth: HashMap::new(),
                system: None,
                depth: rec.depth as u32,
                max_turns: None,
                turn_count: 0,
                cap_summary_injected: false,
                running_turn: None,
                turn_cancel: None,
                turn_tool_filter: None,
                session_started: false,
                plan_mode: false,
                pending_plan_approval: HashMap::new(),
                title_state: TitleState::restore(
                    rec.title,
                    rec.title_override,
                    title_last_eval_user_count,
                ),
                prefix_stability: StablePrefix::default(),
                provider_id,
                parent_id: rec.parent_id,
                archived: rec.archived,
                pinned: rec.pinned,
                created_at: rec.created_at,
                interacted_at: rec.interacted_at,
                token_meter: TokenMeter::restore(
                    rec.cumulative_token_usage,
                    rec.request_token_usage,
                ),
                persist_revision: AtomicU64::new(rec.revision),
            }
        })
    }

    /// Construct a sub-agent `Thread` with a restricted tool registry, an
    /// independent permission cache, a system prompt, and a turn cap. The
    /// `tools_fn` closure receives the new thread's own `WeakEntity` so the
    /// `agent` tool (when nesting is allowed) can route back to it.
    ///
    /// The sub-agent must reuse the *parent's* `AnyLanguageModel` instance so its
    /// LLM requests share the same `prompt_cache_key` (the model's stable id).
    /// This lets the sub-agent's oneshot turns read the prompt-cache prefix the
    /// parent's main loop already populated, instead of cold-missing the whole
    /// prefix. If a future change lets a sub-agent use a different (e.g. cheaper
    /// summarization) model, it must explicitly forward the parent's
    /// `prompt_cache_key` to preserve cache affinity.
    #[allow(clippy::too_many_arguments)]
    pub fn new_subagent(
        cwd: PathBuf,
        model: AnyLanguageModel,
        permission: Arc<PermissionCache>,
        approval_mode: ApprovalMode,
        system: String,
        max_turns: u32,
        depth: u32,
        tools_fn: impl FnOnce(WeakEntity<Self>) -> ToolRegistry,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let weak = cx.weak_entity();
            Self {
                id: ThreadId(uuid::Uuid::new_v4().to_string()),
                messages: Vec::new(),
                model: Some(model),
                tools: Arc::new(tools_fn(weak)),
                permission,
                approval_mode,
                approval_ask_reasons: HashMap::new(),
                cwd,
                project: None,
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
                pending_auth_meta: HashMap::new(),
                pending_child_auth: HashMap::new(),
                system: Some(system),
                depth,
                max_turns: Some(max_turns),
                turn_count: 0,
                cap_summary_injected: false,
                running_turn: None,
                turn_cancel: None,
                turn_tool_filter: None,
                session_started: false,
                plan_mode: false,
                pending_plan_approval: HashMap::new(),
                title_state: TitleState::default(),
                prefix_stability: StablePrefix::default(),
                provider_id: None,
                parent_id: None,
                archived: false,
                pinned: false,
                created_at: chrono::Utc::now().timestamp(),
                interacted_at: chrono::Utc::now().timestamp(),
                token_meter: TokenMeter::default(),
                persist_revision: AtomicU64::new(0),
            }
        })
    }

    /// Build a persistable snapshot (with the first user message as summary). Returns `None` when there is no model (not persisted).
    pub fn snapshot(&self) -> Option<ThreadRecord> {
        let model_id = self.model.as_ref().map(|m| m.id())?;
        // Stamp a fresh, strictly-monotonic revision so a stale fire-and-forget
        // upsert (one whose snapshot predates a newer write already committed)
        // is rejected by `ThreadsDatabase::upsert` instead of clobbering it.
        let revision = self.persist_revision.fetch_add(1, Ordering::Relaxed) + 1;
        Some(ThreadRecord {
            id: self.id.0.clone(),
            summary: self.summary(),
            title: self.title_state.snapshot_title(),
            title_override: self.title_state.snapshot_override(),
            model_id,
            provider_id: self.provider_id.clone(),
            cwd: self.cwd.display().to_string(),
            project: self
                .project
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            approval_mode: self.approval_mode.as_i64(),
            depth: self.depth as i32,
            parent_id: self.parent_id.clone(),
            archived: self.archived,
            pinned: self.pinned,
            created_at: self.created_at,
            interacted_at: self.interacted_at,
            updated_at: chrono::Utc::now().timestamp(),
            // No separate field; the session starts when the thread is created.
            session_started_at: self.created_at,
            revision,
            cumulative_token_usage: self.token_meter.cumulative(),
            messages: self.messages.clone(),
            request_token_usage: self.token_meter.per_request().clone(),
        })
    }

    /// Display title: the LLM-generated title if present, else the mechanical
    /// first-user-message summary. Used both for the sidebar and the persisted
    /// `ThreadRecord::summary`.
    fn summary(&self) -> String {
        self.title_state
            .title()
            .map(str::to_string)
            .unwrap_or_else(|| self.mechanical_summary())
    }

    /// User-facing display title with precedence: user rename > LLM title >
    /// mechanical summary. Mirrors [`crate::db::ThreadSummary::display_title`]
    /// so the title bar (live thread) and sidebar (loaded summaries) agree.
    pub fn display_title(&self) -> String {
        self.title_state
            .display()
            .map(str::to_string)
            .unwrap_or_else(|| self.summary())
    }

    /// Render the conversation as a Markdown transcript. Each message is a
    /// `## User` / `## Assistant` heading followed by its text content. Image
    /// blocks become a `(image)` placeholder; tool_use / tool_result blocks
    /// become fenced code blocks. Used by the title bar menu's "Copy as
    /// Markdown" entry.
    pub fn to_markdown(&self) -> String {
        use crate::language_model::MessageContent;
        let mut out = String::new();
        for m in &self.messages {
            let heading = match m.role {
                Role::User => "## User",
                Role::Assistant => "## Assistant",
                Role::System => "## System",
            };
            out.push_str(heading);
            out.push('\n');
            for c in &m.content {
                match c {
                    MessageContent::Text(t) => {
                        out.push_str(t);
                        out.push('\n');
                    }
                    MessageContent::Thinking { text, .. } => {
                        out.push_str("> *thinking:* ");
                        out.push_str(text);
                        out.push('\n');
                    }
                    MessageContent::Image { .. } => {
                        out.push_str("(image)\n");
                    }
                    MessageContent::ToolUse(u) => {
                        let body = if u.raw_input.is_empty() {
                            u.input.to_string()
                        } else {
                            u.raw_input.clone()
                        };
                        out.push_str(&format!("```tool_use: {}\n{}\n```\n", u.name, body));
                    }
                    MessageContent::ToolResult(r) => {
                        let tag = if r.is_error {
                            "tool_result (error)"
                        } else {
                            "tool_result"
                        };
                        out.push_str(&format!("```{tag}\n{}\n```\n", r.content));
                    }
                }
            }
            out.push('\n');
        }
        out
    }

    /// Mechanical fallback: first user message text, truncated to 60 chars;
    /// falls back to the localized default when absent. Used until the LLM
    /// title stream lands.
    fn mechanical_summary(&self) -> String {
        for m in &self.messages {
            if m.role != Role::User {
                continue;
            }
            let mut text = String::new();
            for c in &m.content {
                if let MessageContent::Text(t) = c {
                    text.push_str(t);
                }
            }
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return truncate_summary(trimmed, 60);
            }
        }
        // No user text yet: return empty so the sidebar renders its localized
        // "(New chat)" placeholder rather than storing a display string in the
        // DB (which would freeze it to whichever language was active at first
        // turn).
        String::new()
    }

    /// Maybe kick off an LLM title stream after a turn. Delegates to
    /// [`TitleState::maybe_generate`], passing this thread's depth / model /
    /// messages as runtime context. See that method for the gating + cadence.
    fn maybe_generate_title(&mut self, cx: &mut Context<Self>) {
        self.title_state
            .maybe_generate(self.depth, self.model.as_ref(), &self.messages, cx);
    }

    /// Called after the UI resolves authorization: route the response to the
    /// matching pending responder by id. Handles three cases:
    /// 1. an id matching this thread's own `pending_authorizations` → send to the tool;
    /// 2. a composite id `<parent_tool_use_id>::<child_auth_id>` registered via
    ///    `register_child_auth` → forward to the child thread;
    /// 3. no match → silently drop (stale, cancelled, or already resolved).
    pub fn respond_authorization(
        &mut self,
        id: &str,
        response: ToolAuthorizationResponse,
        cx: &mut Context<Self>,
    ) {
        if let Some(tx) = self.pending_authorizations.remove(id) {
            self.pending_auth_meta.remove(id);
            let _ = tx.send(response);
            return;
        }
        if let Some(route) = self.pending_child_auth.remove(id) {
            if let Some(child) = route.child.upgrade() {
                let child_id = route.child_auth_id.clone();
                child.update(cx, |c, cx| {
                    c.respond_authorization(&child_id, response, cx);
                });
            }
            return;
        }
        let _ = cx;
    }

    /// Register a sub-agent's authorization request under a composite id and
    /// re-emit it on this (parent) thread so the UI overlay can prompt the user.
    /// The decision later arrives via `respond_authorization` and is forwarded
    /// to the child. Called by the `agent` tool's event subscription.
    #[allow(clippy::too_many_arguments)]
    pub fn register_child_auth(
        &mut self,
        composite_id: String,
        child: WeakEntity<Thread>,
        child_auth_id: String,
        tool_name: String,
        summary: String,
        input: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        self.pending_child_auth.insert(
            composite_id.clone(),
            ChildAuthRoute {
                child,
                child_auth_id,
            },
        );
        cx.emit(ThreadEvent::ToolCallAuthorization {
            id: composite_id,
            tool_name,
            summary,
            input,
        });
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Whether this thread is currently in plan mode.
    pub fn plan_mode(&self) -> bool {
        self.plan_mode
    }

    /// Toggle plan mode. Called by the UI (slash `/plan` or the `+` menu row).
    /// Only sets the flag; the next `build_completion_request` filters tools and
    /// appends the plan-mode addendum. Does not start a turn.
    pub fn set_plan_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        self.plan_mode = on;
        cx.notify();
    }

    /// Resolve a pending plan approval (user clicked approve/reject in the UI
    /// overlay). Mirrors `respond_authorization` for the plan-approval oneshot.
    pub fn respond_plan_approval(
        &mut self,
        id: &str,
        response: PlanApprovalResponse,
        cx: &mut Context<Self>,
    ) {
        if let Some(tx) = self.pending_plan_approval.remove(id) {
            let _ = tx.send(response);
        }
        let _ = cx;
    }

    /// Snapshot of this Thread's always-allow set, for seeding a sub-agent's
    /// permission cache so the child does not re-prompt grants the user already
    /// gave the parent for the same tool.
    pub fn permission_snapshot(&self) -> std::collections::HashSet<String> {
        self.permission.allowed_tools()
    }

    /// Current approval mode. Drives the access chip, the popover, and the
    /// free / approval branch in `run_turn_loop`.
    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
    }

    /// Switch approval mode. Emits [`ThreadEvent::ApprovalModeChanged`] so the
    /// UI refreshes the chip, the popover's selected row, and (for Yolo) the
    /// seatbelt branch in the next tool call. No-op when the mode is unchanged.
    pub fn set_approval_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        if self.approval_mode == mode {
            return;
        }
        self.approval_mode = mode;
        cx.emit(ThreadEvent::ApprovalModeChanged { mode });
        cx.notify();
    }

    /// Whether the user pinned this thread from the title bar menu. Pinned
    /// threads float to the top of the sidebar list.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the user archived this thread from the sidebar.
    pub fn archived(&self) -> bool {
        self.archived
    }

    /// Toggle the pinned flag. Persistence is the caller's responsibility
    /// (the workspace calls `ThreadStore::pin_thread` which writes to the DB
    /// and refreshes the sidebar list).
    pub fn set_pinned(&mut self, pinned: bool, cx: &mut Context<Self>) {
        if self.pinned == pinned {
            return;
        }
        self.pinned = pinned;
        cx.notify();
    }

    /// Toggle the archived flag. Persistence is the caller's responsibility
    /// (the workspace calls `ThreadStore::archive_thread` which writes to the
    /// DB and refreshes the sidebar list).
    pub fn set_archived(&mut self, archived: bool, cx: &mut Context<Self>) {
        if self.archived == archived {
            return;
        }
        self.archived = archived;
        cx.notify();
    }

    /// Take a reason the auto-review agent attached to an `Ask` verdict,
    /// removing it so a follow-up call with the same id is empty. Used by the
    /// auth overlay to render the justification under the tool title.
    pub fn take_approval_ask_reason(&mut self, id: &str) -> Option<String> {
        self.approval_ask_reasons.remove(id)
    }

    pub fn model(&self) -> Option<&AnyLanguageModel> {
        self.model.as_ref()
    }

    pub fn set_model(&mut self, model: AnyLanguageModel, cx: &mut Context<Self>) {
        let from = self.model.as_ref().map(|m| m.id());
        let to = model.id();
        self.provider_id = Some(model.provider_id());
        self.model = Some(model);
        cx.emit(ThreadEvent::ModelChanged { from, to });
        cx.notify();
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// True when at least one user message has been appended — i.e. the user
    /// has submitted real input to the model. Unpersisted empty threads (the
    /// initial "new conversation" screen) return `false`.
    pub fn has_interacted(&self) -> bool {
        self.messages.iter().any(|m| m.role == Role::User)
    }

    /// Id of the last `Role::User` message, if any. Used as the key for
    /// per-request token usage: each LLM round in a tool-use loop is attributed
    /// to the user message that triggered the turn (tool results are `User`).
    fn last_user_message_id(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.id.as_str())
    }

    /// Cumulative token usage across the whole thread's life.
    pub fn cumulative_token_usage(&self) -> TokenUsage {
        self.token_meter.cumulative()
    }

    /// Per-user-message usage, keyed by `Message::id`.
    pub fn request_token_usage(&self) -> &HashMap<String, TokenUsage> {
        self.token_meter.per_request()
    }

    /// Token usage attributed to the last user message, if the provider
    /// reported any for this turn. Used by the UI to label the assistant reply
    /// that the turn produced.
    pub fn last_request_token_usage(&self) -> Option<TokenUsage> {
        self.token_meter.last_request(self.last_user_message_id())
    }

    /// Per-model cumulative token usage, keyed by model display name.
    pub fn per_model_token_usage(&self) -> &HashMap<String, TokenUsage> {
        self.token_meter.per_model()
    }

    /// Fold a streaming `UsageUpdate` into the meter. The caller emits the
    /// cumulative on `ThreadEvent::TokenUsageUpdated` after this returns.
    fn accumulate_token_usage(&mut self, new: TokenUsage) {
        if let Some(model) = self.model.as_ref() {
            self.token_meter.accumulate_for_model(new, &model.name());
        } else {
            self.token_meter.accumulate(new);
        }
    }

    /// Attribute the in-flight request's usage to its triggering user message
    /// and reset the per-request counter. Called on every terminal path —
    /// `Stop` from the provider and `cancel()` from the user — so a cancelled
    /// turn still lands its partial usage and the next turn starts from zero
    /// instead of diffing against a stale counter.
    fn finalize_request_usage(&mut self) {
        let uid = self.last_user_message_id().map(str::to_owned);
        self.token_meter.finalize_request(uid.as_deref());
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn project(&self) -> Option<&PathBuf> {
        self.project.as_ref()
    }

    pub fn turn_count(&self) -> u32 {
        self.turn_count
    }

    pub fn max_turns(&self) -> Option<u32> {
        self.max_turns
    }

    /// Bind the thread to a project directory, rebuilding the tool registry so
    /// tools resolve paths against it. A no-op once the conversation has started
    /// (project is chosen on the empty first screen and fixed thereafter).
    pub fn set_project(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !self.messages.is_empty() {
            return;
        }
        self.cwd = dir.clone();
        self.tools = Arc::new(tools::main_registry(dir.clone(), cx.weak_entity()));
        self.project = Some(dir);
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        self.running_turn.is_some()
    }

    /// Return the UI metadata for pending authorizations. Used by the workspace
    /// to re-emit `ToolCallAuthorization` events when switching back to a thread
    /// that was parked waiting for user approval.
    pub fn pending_auth_entries(&self) -> Vec<(String, &PendingAuthMeta)> {
        self.pending_authorizations
            .keys()
            .filter_map(|id| {
                self.pending_auth_meta
                    .get(id)
                    .map(|meta| (id.clone(), meta))
            })
            .collect()
    }

    /// Append a user message.
    pub fn insert_user_message(&mut self, text: String, cx: &mut Context<Self>) {
        self.messages.push(Message::user(text));
        cx.notify();
    }

    /// Append a user message carrying multiple content blocks (e.g. text plus attached images).
    pub fn insert_user_message_with_content(
        &mut self,
        content: Vec<MessageContent>,
        cx: &mut Context<Self>,
    ) {
        self.messages.push(Message::user_with_content(content));
        cx.notify();
    }

    /// Resolve and run a slash command. The command body (with `$ARGUMENTS`
    /// substituted) is appended as a user message, and the command's
    /// `allowed-tools` whitelist narrows the turn's tool set for the duration
    /// of the run. Returns `false` when no command named `name` is registered,
    /// leaving the thread untouched so the UI can surface an error. `name` may
    /// be `plugin:command` or a bare `command`.
    pub fn submit_command(&mut self, name: &str, args: &str, cx: &mut Context<Self>) -> bool {
        let Some(cmd) = crate::command::global().get(name).cloned() else {
            return false;
        };
        let rendered = cmd.render(args);
        if !cmd.allowed_tools.is_empty() {
            self.turn_tool_filter = Some(cmd.allowed_tools.clone());
        }
        self.insert_user_message(rendered, cx);
        self.run_turn(cx);
        true
    }

    /// Start a completion turn. No-ops when a turn is already running or there is no model.
    pub fn run_turn(&mut self, cx: &mut Context<Self>) {
        if self.running_turn.is_some() {
            return;
        }
        let Some(model) = self.model.clone() else {
            cx.emit(ThreadEvent::Error(anyhow::anyhow!("No model configured")));
            return;
        };

        // Signal the UI immediately that a turn is in flight — before the
        // first streaming delta arrives — so the sidebar running indicator
        // lights up during the warm-up gap. `ThreadStore::set_running` (called
        // by the workspace on this event) is the bridge to the sidebar.
        cx.emit(ThreadEvent::TurnStarted);

        let cancel = CancellationToken::new();
        self.turn_cancel = Some(cancel.clone());

        let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = Self::run_turn_loop(&this, &model, &cancel, cx).await;
            if let Err(e) = result {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(ThreadEvent::Error(e));
                });
            }
            this.update(cx, |this, cx| {
                this.running_turn = None;
                this.turn_cancel = None;
                // A slash command's tool filter lasts only for its turn; clear it
                // so a subsequent free-form message inherits the full tool set.
                this.turn_tool_filter = None;
                // A natural (non-cancelled) turn end is the trigger point for
                // title re-evaluation. `maybe_generate_title` self-gates on
                // depth, cadence, and dedup; cancelled turns skip it.
                if !cancel.is_cancelled() {
                    this.maybe_generate_title(cx);
                }
                cx.notify();
            })
            .ok();
            // Persistence backstop: `Stop`/`Error` events are the workspace's
            // signal to save, but a stream that ends without a `MessageStop`
            // (provider hiccup, non-SSE response from a compatibility endpoint)
            // makes run_turn_loop return Ok with no terminal event emitted, so
            // the workspace never learns the turn ended and the assistant
            // content accumulated in `messages` is lost on the next thread
            // switch. Persist here unconditionally — the revision guard makes
            // this safe alongside any overlapping event-driven saves, and a
            // dropped entity (the workspace already switched away) just skips.
            if let Some(entity) = this.upgrade() {
                cx.update(|cx: &mut gpui::App| {
                    crate::thread_store::save_thread(entity, true, cx);
                });
            }
        });

        self.running_turn = Some(task);
        cx.notify();
    }

    /// Abort the current turn. Cancels the turn token so an in-flight tool (e.g.
    /// `bash`) can kill its process group and append a clean "aborted" result;
    /// the turn task then winds down on its own and clears `running_turn`.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.turn_cancel.take() {
            cancel.cancel();
            // A cancelled slash-command turn must also clear its tool filter.
            self.turn_tool_filter = None;
            // Drop pending plan-approval oneshots immediately so the senders
            // are closed. Without this, the async `run_plan_approval` task
            // stays parked until the entity is dropped, and a late
            // `respond_plan_approval` from the UI silently no-ops.
            self.pending_plan_approval.clear();
            // Symmetric cleanup of pending tool-authorization oneshots. A
            // cancelled turn resolves in-flight approvals via the cancellation
            // token's `select!` arms, but the senders must also be dropped so a
            // late `respond_authorization` from the UI silently no-ops instead
            // of panicking on a closed channel.
            self.pending_authorizations.clear();
            self.pending_auth_meta.clear();
            // Drop bubbled sub-agent auth routes too: a parent cancel must
            // unwind child-authorization routing so a late composite-id
            // response no-ops at the parent instead of traversing to a child
            // whose own pending map is already empty.
            self.pending_child_auth.clear();
            // Attribute partial usage from the cancelled turn and reset the
            // per-request counter so the next turn's delta starts from zero.
            self.finalize_request_usage();
            cx.emit(ThreadEvent::Stop(StopReason::EndTurn));
            cx.notify();
        }
    }

    async fn run_turn_loop(
        this: &gpui::WeakEntity<Self>,
        model: &AnyLanguageModel,
        cancel: &CancellationToken,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        // Fire SessionStart once for a main thread (depth 0) before its first
        // turn. Sub-agents (depth > 0) never fire it — they are scoped to a
        // single delegation, not a user session.
        this.update(cx, |this, _cx| {
            if this.depth == 0 && !this.session_started {
                this.session_started = true;
                let thread_id = this.id.0.clone();
                let cwd = this.cwd.display().to_string();
                crate::hook::fire(
                    crate::hook::HookEvent::SessionStart,
                    Some(&cwd),
                    serde_json::json!({"thread_id": thread_id, "cwd": cwd}),
                );
            }
        })?;
        // `turn_count` tracks the round-trip index within this `run_turn` only,
        // not a session-wide total — reset on entry so `self_info` reports the
        // current turn's progress (issue 5: it used to accumulate across user
        // messages, reading "14/unlimited" mid-session). `cap_summary_injected`
        // resets in lockstep so a prior turn's cap does not suppress this one's.
        this.update(cx, |this, _| {
            this.turn_count = 0;
            this.cap_summary_injected = false;
        })?;
        loop {
            // Increment at the top so `turn_count` is 1-indexed for the
            // round-trip about to run — `self_info` mid-batch reads the current
            // index, not the previous batch's count.
            this.update(cx, |this, _| this.turn_count += 1)?;
            let request = this.update(cx, |this, cx| {
                this.pending_tool_uses.clear();
                this.reconcile_tool_uses(cx);
                this.build_completion_request()
            })?;

            // Fingerprint the request's system prompt + tool specs against the
            // previous turn so prefix drift (history rewrite, tool hot-reload,
            // plan-mode toggle) is observable. Emitted every turn with the
            // current stability ratio; drift flags are `true` only when that
            // component changed this turn.
            this.update(cx, |this, cx| {
                let change = this.prefix_stability.build(&request);
                cx.emit(ThreadEvent::PrefixStability {
                    stability_pct: this.prefix_stability.stability_pct(),
                    system_changed: change.is_some_and(|c| c.system_changed),
                    tools_changed: change.is_some_and(|c| c.tools_changed),
                });
            })?;

            let mut stream = tokio::select! {
                s = model.stream_completion(request, cx) => s?,
                _ = cancel.cancelled() => break,
            };
            let mut cancelled = false;
            loop {
                // Take one event (yielding until ready) so the loop always makes
                // progress, then drain every further already-ready event in the
                // same tick via `now_or_never`. Applying a batch inside a single
                // `update` amortizes the gpui executor crossing and lets sibling
                // deltas coalesce into one layout pass.
                let first = tokio::select! {
                    ev = stream.next() => match ev {
                        Some(e) => e,
                        None => break,
                    },
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    }
                };
                let mut batch = vec![first];
                while let Some(Some(ev)) = stream.next().now_or_never() {
                    batch.push(ev);
                }
                let has_stop = batch
                    .iter()
                    .any(|e| matches!(e, Ok(LanguageModelCompletionEvent::Stop(_))));
                this.update(cx, |this, cx| {
                    for ev in batch {
                        this.handle_completion_event(ev, cx);
                    }
                })?;
                // Yield exactly once to the gpui foreground executor between
                // batches so the macOS run loop can drain queued runnables
                // (input events, frame callbacks). A fast-streaming provider
                // keeps the async_channel non-empty, so `poll_next` returns
                // Ready without registering a waker; without this yield the
                // turn task starves the run loop and the UI freezes mid-turn.
                //
                // The future must resolve after a single yield — a perpetual
                // `Pending` would hang the turn after the first batch (every
                // subsequent delta, text chunk, and the Stop would sit unread
                // in the channel). `yielded` ensures the second poll returns
                // `Ready`, so control returns to the drain loop below.
                let mut yielded = false;
                std::future::poll_fn(move |cx: &mut std::task::Context<'_>| {
                    if yielded {
                        std::task::Poll::Ready(())
                    } else {
                        yielded = true;
                        cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                })
                .await;
                if has_stop {
                    break;
                }
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
            }
            if cancelled {
                break;
            }

            // Categorize tool_uses into free / approval / auto-review queues.
            // The auto-review queue is settled in a second pass below — we
            // need a model handle and the cancel token, so it cannot be
            // decided inside the borrow of `this`.
            let (
                tool_uses,
                mut free_tus,
                mut approval_tus,
                auto_review_tus,
                model,
                cancel_for_review,
                cwd,
            ) = this.update(cx, |this, _cx| {
                let tool_uses = std::mem::take(&mut this.pending_tool_uses);
                let mut free = Vec::new();
                let mut appr = Vec::new();
                let mut review = Vec::new();
                for tu in &tool_uses {
                    // `exit_plan_mode` is synthesized (not in `this.tools`), so
                    // the registry lookup below would default it to the free
                    // (parallel) path; it has its own plan-overlay routing.
                    let is_plan_exit = tu.name.as_ref() == "exit_plan_mode";
                    // YOLO/AutoReview bypass the permission gate, but not tools
                    // whose authorization flow IS their execution
                    // (AskUserQuestion): bypassing those would drop the user's
                    // input and hit an unreachable `run`. AutoReview also
                    // consults the security-reviewer agent before allowing.
                    let tool = this.tools.get(tu.name.as_ref());
                    let requires_approval = tool
                        .map(|t| t.requires_approval(&tu.input))
                        .unwrap_or(false);
                    let requires_user_input =
                        tool.map(|t| t.requires_user_input()).unwrap_or(false);
                    let always_allowed = this.permission.is_always_allowed(tu.name.as_ref());
                    let bypassed = matches!(
                        this.approval_mode,
                        ApprovalMode::Yolo | ApprovalMode::AutoReview
                    ) && !requires_user_input;

                    if is_plan_exit {
                        appr.push(tu.clone());
                    } else if !requires_approval || always_allowed {
                        free.push(tu.clone());
                    } else if bypassed && this.approval_mode == ApprovalMode::Yolo {
                        // Yolo short-circuits; AutoReview must consult the
                        // reviewer below.
                        free.push(tu.clone());
                    } else if bypassed {
                        review.push(tu.clone());
                    } else {
                        appr.push(tu.clone());
                    }
                }
                // Reset per-turn ask-reason map so a previous turn's
                // reasons never bleed into the current one if a tool id
                // collides across turns.
                this.approval_ask_reasons.clear();
                let model = this.model.clone();
                let cancel = this
                    .turn_cancel
                    .clone()
                    .unwrap_or_else(CancellationToken::new);
                (
                    tool_uses,
                    free,
                    appr,
                    review,
                    model,
                    cancel,
                    this.cwd.clone(),
                )
            })?;
            if tool_uses.is_empty() {
                break;
            }

            // AutoReview: ask the security-reviewer agent for each pending call.
            // Allow verdicts flow into `free_tus`; Ask verdicts flow into
            // `approval_tus` with a one-line reason surfaced in the overlay.
            if !auto_review_tus.is_empty() {
                match model.as_ref() {
                    None => {
                        // No model loaded — defer everything to the overlay so
                        // the user is never silently auto-approved without a
                        // model in the loop.
                        approval_tus.extend(auto_review_tus);
                    }
                    Some(model) => {
                        for tu in auto_review_tus {
                            if cancel_for_review.is_cancelled() {
                                approval_tus.push(tu);
                                continue;
                            }
                            let title = tool_title(&tu.name, &tu.input);
                            let verdict = crate::approval::review(
                                model,
                                &tu.name,
                                &tu.input,
                                &title,
                                &cwd,
                                cancel_for_review.clone(),
                                cx,
                            )
                            .await;
                            match verdict {
                                crate::approval::ReviewVerdict::Allow => free_tus.push(tu),
                                crate::approval::ReviewVerdict::Ask { reason } => {
                                    let id = tu.id.clone();
                                    let _ = this.update(cx, |this, _cx| {
                                        this.approval_ask_reasons.insert(id, reason);
                                    });
                                    approval_tus.push(tu);
                                }
                            }
                        }
                    }
                }
            }

            // Approval-free tools run in parallel; approval-needed tools run
            // serially so the single-slot UI auth overlay never holds two
            // pending prompts at once (the second would overwrite the first and
            // strand the first tool's `oneshot` forever). gpui `Task`s are
            // `!Send`, so concurrency is "spawn all, then await in order".
            let free_tasks: Vec<Task<Result<()>>> = this.update(cx, |_this, cx| {
                free_tus
                    .iter()
                    .cloned()
                    .map(|tu| {
                        let cancel = cancel.clone();
                        cx.spawn(async move |this, cx: &mut AsyncApp| {
                            Self::run_tool_inner(this, tu, cancel, cx).await
                        })
                    })
                    .collect()
            })?;

            let mut fulfilled: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut first_err: Option<anyhow::Error> = None;
            for (i, task) in free_tasks.into_iter().enumerate() {
                match task.await {
                    Ok(()) => {
                        fulfilled.insert(free_tus[i].id.clone());
                    }
                    Err(e) if first_err.is_none() => {
                        first_err = Some(e);
                    }
                    Err(_) => {}
                }
                if cancel.is_cancelled() {
                    break;
                }
            }

            // Approval-needed tools run one at a time; a cancelled turn or a
            // sibling error skips the rest (they are synthesized below).
            // Fail-fast on a sibling error mirrors the pre-parallel serial
            // loop: an infrastructure error aborts the turn rather than
            // continuing to prompt the user for tools that may be running in
            // a half-broken state.
            for tu in approval_tus {
                if cancel.is_cancelled() || first_err.is_some() {
                    break;
                }
                let tu_id = tu.id.clone();
                let task: Task<Result<()>> = this.update(cx, |_this, cx| {
                    let cancel = cancel.clone();
                    cx.spawn(async move |this, cx: &mut AsyncApp| {
                        Self::run_tool_inner(this, tu, cancel, cx).await
                    })
                })?;
                match task.await {
                    Ok(()) => {
                        fulfilled.insert(tu_id);
                    }
                    Err(e) if first_err.is_none() => {
                        first_err = Some(e);
                    }
                    Err(_) => {}
                }
            }

            // Cancel or error may have left tool_uses without a paired
            // tool_result. Anthropic requires every tool_use to have one or the
            // next request 400s, so synthesize an error result for any unrun id.
            for tu in &tool_uses {
                if !fulfilled.contains(&tu.id) {
                    Self::synthesize_unrun_tool_result(this, tu.clone(), cx)?;
                }
            }

            if let Some(e) = first_err {
                return Err(e);
            }
            if cancel.is_cancelled() {
                break;
            }

            // Sub-agent turn cap: stop runaway sub-agents after `max_turns`
            // round-trips. `turn_count` is 1-indexed and incremented at the top
            // of each round-trip, so `>= max` fires at the end of the Nth
            // round-trip — after exactly `max` tool round-trips have run. The
            // first hit injects one summary turn so the sub-agent can wrap up
            // with a coherent final message instead of ending mid-work; a
            // second hit (the summary turn itself overflowed with tools)
            // hard-stops.
            let hit_cap = this.update(cx, |this, cx| {
                let Some(max) = this.max_turns else {
                    return false;
                };
                let capped = this.turn_count >= max;
                if capped && !this.cap_summary_injected {
                    this.cap_summary_injected = true;
                    this.insert_user_message(
                        crate::system_prompt::max_turns_summary_prompt(max),
                        cx,
                    );
                    return false;
                }
                if capped {
                    cx.emit(ThreadEvent::Stop(StopReason::EndTurn));
                }
                capped
            })?;
            if hit_cap {
                break;
            }
        }
        Ok(())
    }

    /// Run a single tool call: authorize (if needed) → run → append a ToolResult message → emit.
    /// Owned `WeakEntity`/`CancellationToken` so each parallel tool runs in its own spawned task.
    async fn run_tool_inner(
        this: gpui::WeakEntity<Self>,
        tu: LanguageModelToolUse,
        cancel: CancellationToken,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let id = tu.id.clone();
        let name = tu.name.to_string();
        let title = tool_title(&name, &tu.input);

        // Plan-mode exit handshake: the model submitted a plan. Intercept before
        // the registry lookup (`exit_plan_mode` is synthesized in
        // `build_completion_request`, not registered) and run the approval flow.
        if name == "exit_plan_mode" && this.read_with(cx, |this, _| this.plan_mode)? {
            return Self::run_plan_approval(this, tu, cancel, cx).await;
        }

        let tool = this.read_with(cx, |this, _| this.tools.get(&name).cloned())?;

        let Some(tool) = tool else {
            let msg = format!("Unknown tool: {name}");
            Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
            Self::append_tool_result(&this, tu, msg.clone(), true, cx)?;
            return Ok(());
        };

        // Plan-mode write backstop: a write tool should never be advertised in
        // plan mode (the request-tool list is filtered), but if one slips
        // through (stale registry, model hallucination) synthesize an error
        // rather than execute it.
        let in_plan = this.read_with(cx, |this, _| this.plan_mode)?;
        if in_plan && !tool.is_read_only() {
            let msg = format!("Plan mode does not permit calling {name}.");
            Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
            Self::append_tool_result(&this, tu, msg, true, cx)?;
            return Ok(());
        }

        // YOLO/AutoReview bypasses the permission gate: skip the authorization
        // prompt entirely so the tool runs immediately. YOLO is the
        // session-level equivalent of codex's `approval_policy = Never`;
        // AutoReview additionally consults the security-reviewer agent before
        // allowing. Tools whose authorization flow IS their execution
        // (`AskUserQuestion`) are exempt: bypassing them would drop the user's
        // input and hit an unreachable `run`.
        let requires_user_input = tool.requires_user_input();
        let needs_approval = tool.requires_approval(&tu.input)
            && !this.read_with(cx, |this, _| {
                (matches!(
                    this.approval_mode,
                    ApprovalMode::Yolo | ApprovalMode::AutoReview
                ) && !requires_user_input)
                    || this.permission.is_always_allowed(&name)
            })?;
        if needs_approval {
            this.update(cx, |_, cx| {
                cx.emit(ThreadEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    title: title.clone(),
                    status: ToolCallStatus::PendingApproval,
                });
            })?;

            let (tx, rx) = tokio::sync::oneshot::channel();
            this.update(cx, |this, _cx| {
                this.pending_authorizations.insert(id.clone(), tx);
                this.pending_auth_meta.insert(
                    id.clone(),
                    PendingAuthMeta {
                        tool_name: name.clone(),
                        summary: title.clone(),
                        input: tu.input.clone(),
                    },
                );
            })?;
            this.update(cx, |_, cx| {
                cx.emit(ThreadEvent::ToolCallAuthorization {
                    id: id.clone(),
                    tool_name: name.clone(),
                    summary: title.clone(),
                    input: tu.input.clone(),
                });
            })?;

            let response = tokio::select! {
                r = rx => r.unwrap_or(ToolAuthorizationResponse::Decision(PermissionDecision::Deny)),
                _ = cancel.cancelled() => ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
            };
            // The pending responder is spent whether the UI answered or cancel
            // fired; remove it so a late `respond_authorization` cannot revive a
            // cancelled turn.
            this.update(cx, |this, _cx| {
                this.pending_authorizations.remove(&id);
                this.pending_auth_meta.remove(&id);
            })?;
            match response {
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny) => {
                    let msg = "User denied execution".to_string();
                    Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
                    Self::append_tool_result(&this, tu, msg, true, cx)?;
                    return Ok(());
                }
                ToolAuthorizationResponse::Decision(PermissionDecision::AlwaysAllow) => {
                    this.read_with(cx, |this, _| this.permission.set_always_allowed(&name))?;
                }
                ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce) => {}
                ToolAuthorizationResponse::AskUserQuestion { answers, response } => {
                    let output = match response {
                        Some(text) => format!("User responded: {text}"),
                        None => {
                            let mut buf = String::new();
                            for (q, a) in &answers {
                                buf.push_str("Question: ");
                                buf.push_str(q);
                                buf.push_str("\nAnswer: ");
                                buf.push_str(a);
                                buf.push_str("\n\n");
                            }
                            buf.trim_end().to_string()
                        }
                    };
                    Self::emit_tool_result(&this, &id, &name, &title, &output, false, cx)?;
                    Self::append_tool_result(&this, tu, output, false, cx)?;
                    return Ok(());
                }
            }
        }

        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                title: title.clone(),
                status: ToolCallStatus::Running,
            });
        })?;

        // PreToolUse hooks fire after approval but before execution — the
        // handler sees the resolved input and can observe/log the call.
        // Notification-only: a non-zero exit cannot block the tool (fail-open).
        let hook_cwd = this
            .read_with(cx, |t, _| t.cwd.display().to_string())
            .unwrap_or_default();
        crate::hook::fire(
            crate::hook::HookEvent::PreToolUse,
            Some(&hook_cwd),
            serde_json::json!({
                "thread_id": this.read_with(cx, |t, _| t.id.0.clone()).unwrap_or_default(),
                "tool_name": name,
                "tool_use_id": id,
                "input": tu.input,
            }),
        );

        let input = tu.input.clone();
        let (sink, rx) = crate::tool::ToolOutputSink::channel(id.clone().into());
        let id_for_drain = id.clone();
        // Snapshot the read-only runtime identity once, before invocation. Tools
        // read it off the `&dyn ToolContext` passed to `run_streaming` instead of
        // holding a `WeakEntity<Thread>` and re-entering the entity from `run` —
        // so the tool call itself never needs the Thread leased on its behalf.
        // Invoke via `cx.update` (App context, no entity lease) rather than
        // `this.update`: a write lease here would still block the drain spawn
        // below from re-entering the entity, and historically tripped gpui's
        // `double_lease_panic` when tools did their own `read_with`.
        let ctx = this.read_with(cx, |t, _| crate::tool::ToolContextSnapshot::from_thread(t))?;
        let result_task: Task<Result<String, String>> =
            cx.update(|cx| tool.run_streaming(input, cancel.clone(), sink, &ctx, cx));
        // Drain live output chunks to the UI while the tool runs. A foreground
        // spawn is used (not background_spawn) because emitting requires an
        // `AsyncApp`, which is `!Send` (`Rc`-backed). The receiver closes once
        // the tool task drops the sink, so this detaches cleanly when
        // `result_task` completes.
        this.update(cx, |_, cx| {
            cx.spawn(async move |this, cx: &mut AsyncApp| {
                while let Ok(chunk) = rx.recv().await {
                    let _ = this.update(cx, |_, cx| {
                        cx.emit(ThreadEvent::ToolOutput {
                            id: id_for_drain.clone(),
                            chunk,
                        });
                    });
                }
            })
            .detach();
        })?;
        let output = result_task.await;
        let (output_str, is_error) = match output {
            Ok(o) => (o, false),
            Err(e) => (e, true),
        };

        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                title: title.clone(),
                status: if is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Success
                },
            });
        })?;

        Self::emit_tool_result(&this, &id, &name, &title, &output_str, is_error, cx)?;
        Self::append_tool_result(&this, tu, output_str.clone(), is_error, cx)?;

        // PostToolUse hooks fire after the result is recorded. The handler sees
        // the output and error flag; fail-open means a hook failure cannot
        // retroactively fail the tool.
        let post_cwd = this
            .read_with(cx, |t, _| t.cwd.display().to_string())
            .unwrap_or_default();
        crate::hook::fire(
            crate::hook::HookEvent::PostToolUse,
            Some(&post_cwd),
            serde_json::json!({
                "thread_id": this.read_with(cx, |t, _| t.id.0.clone()).unwrap_or_default(),
                "tool_name": name,
                "tool_use_id": id,
                "output": output_str,
                "is_error": is_error,
            }),
        );
        Ok(())
    }

    /// Handle an `exit_plan_mode` tool call: extract the plan text, emit
    /// `ThreadEvent::PlanProposed`, park on a oneshot until the user responds
    /// via `respond_plan_approval`. Mirrors the authorization handshake in
    /// `run_tool_inner`. Approve → exit plan mode and end the turn (the user
    /// sends the next message to begin executing); Reject → stay in plan mode
    /// and feed a revision prompt back so the model revises.
    async fn run_plan_approval(
        this: gpui::WeakEntity<Self>,
        tu: LanguageModelToolUse,
        cancel: CancellationToken,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let id = tu.id.clone();
        let name = "exit_plan_mode".to_string();
        let title = "Submit plan".to_string();

        let plan_text = tu
            .input
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or("(no plan text provided)")
            .to_string();

        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                title: title.clone(),
                status: ToolCallStatus::PendingApproval,
            });
        })?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        this.update(cx, |this, _cx| {
            this.pending_plan_approval.insert(id.clone(), tx);
        })?;
        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::PlanProposed {
                id: id.clone(),
                plan_text: plan_text.clone(),
            });
        })?;

        let response = tokio::select! {
            r = rx => r.unwrap_or(PlanApprovalResponse::Reject),
            _ = cancel.cancelled() => PlanApprovalResponse::Reject,
        };
        this.update(cx, |this, _cx| {
            this.pending_plan_approval.remove(&id);
        })?;

        match response {
            PlanApprovalResponse::Approve => {
                let msg = format!(
                    "User approved the plan. Plan contents:\n\n{plan_text}\n\nYou may now begin execution."
                );
                this.update(cx, |_, cx| {
                    cx.emit(ThreadEvent::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        title: title.clone(),
                        status: ToolCallStatus::Success,
                    });
                })?;
                Self::emit_tool_result(&this, &id, &name, &title, &msg, false, cx)?;
                Self::append_tool_result(&this, tu, msg, false, cx)?;
                // Exit plan mode AFTER the tool result is safely appended to
                // the message list. Clearing `plan_mode` before `append` would
                // leave the messages inconsistent (plan mode off but no
                // approval result recorded) if `append_tool_result` failed.
                // The turn loop continues naturally: the next
                // `build_completion_request` sees `plan_mode == false` so the
                // write tools become visible, and the approval ToolResult
                // ("You may now begin execution.") prompts the model to act.
                this.update(cx, |this, cx| {
                    this.plan_mode = false;
                    cx.notify();
                })?;
            }
            PlanApprovalResponse::Reject => {
                let msg = "User rejected the plan. Revise it per the user's feedback and call exit_plan_mode again."
                    .to_string();
                this.update(cx, |_, cx| {
                    cx.emit(ThreadEvent::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        title: title.clone(),
                        status: ToolCallStatus::Denied,
                    });
                })?;
                Self::emit_tool_result(&this, &id, &name, &title, &msg, false, cx)?;
                Self::append_tool_result(&this, tu, msg, false, cx)?;
            }
        }
        Ok(())
    }

    /// Append an error `ToolResult` for a tool_use that never ran (the turn was
    /// cancelled or a sibling tool errored before it started). Anthropic rejects
    /// a request whose tool_uses lack matching tool_results, so this keeps the
    /// message list well-formed when a turn aborts mid-batch.
    fn synthesize_unrun_tool_result(
        this: &gpui::WeakEntity<Self>,
        tu: LanguageModelToolUse,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let id = tu.id.clone();
        let name = tu.name.to_string();
        let title = tool_title(&name, &tu.input);
        let msg = "Tool not executed (session cancelled)".to_string();
        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                title: title.clone(),
                status: ToolCallStatus::Denied,
            });
        })?;
        Self::emit_tool_result(this, &id, &name, &title, &msg, true, cx)?;
        Self::append_tool_result(this, tu, msg, true, cx)?;
        Ok(())
    }

    fn emit_tool_result(
        this: &gpui::WeakEntity<Self>,
        id: &str,
        _name: &str,
        _title: &str,
        output: &str,
        is_error: bool,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolResult {
                id: id.to_string(),
                output: output.to_string(),
                is_error,
            });
        })?;
        Ok(())
    }

    /// Apply a single completion event: accumulate into the assistant message, emit a `ThreadEvent`, and collect tool_uses.
    fn handle_completion_event(
        &mut self,
        event: Result<LanguageModelCompletionEvent>,
        cx: &mut Context<Self>,
    ) {
        match event {
            Ok(LanguageModelCompletionEvent::Text(text)) => {
                self.append_assistant_text(text.clone(), cx);
                cx.emit(ThreadEvent::AgentText(text));
            }
            Ok(LanguageModelCompletionEvent::Thinking { text, signature }) => {
                self.append_assistant_thinking(text.clone(), signature, cx);
                cx.emit(ThreadEvent::AgentThinking(text));
            }
            Ok(LanguageModelCompletionEvent::UsageUpdate(usage)) => {
                self.accumulate_token_usage(usage);
                cx.emit(ThreadEvent::TokenUsageUpdated(
                    self.cumulative_token_usage(),
                ));
            }
            Ok(LanguageModelCompletionEvent::Stop(reason)) => {
                self.finalize_assistant_message(cx);
                // Attribute the just-finished request's usage to its triggering
                // user message, then reset the per-request counter so the next
                // round (a tool-use loop iteration or a new user turn) starts
                // from zero.
                self.finalize_request_usage();
                // Fire Stop hooks (e.g. a stop-gate reviewer) fail-open; the
                // turn has already ended, so the handler runs detached.
                let stop_cwd = self.cwd.display().to_string();
                crate::hook::fire(
                    crate::hook::HookEvent::Stop,
                    Some(&stop_cwd),
                    serde_json::json!({
                        "thread_id": self.id.0,
                        "stop_reason": format!("{reason:?}"),
                    }),
                );
                cx.emit(ThreadEvent::Stop(reason));
            }
            Ok(LanguageModelCompletionEvent::ToolUse(tu)) => {
                // Only persist/enqueue once the input is complete (ContentBlockStop).
                // The mapper also emits ToolUse events on successful incremental
                // InputJsonDelta parses (is_input_complete=false) for live UI preview;
                // those are ignored here, otherwise the same tool would be enqueued
                // multiple times and the assistant message would hold duplicate ToolUse blocks.
                if tu.is_input_complete {
                    self.append_assistant_tool_use(tu.clone(), cx);
                    self.pending_tool_uses.push(tu);
                }
            }
            Ok(LanguageModelCompletionEvent::ToolUseJsonParseError {
                id,
                tool_name,
                raw_input,
                json_parse_error,
            }) => {
                // Insert a placeholder ToolUse block into the assistant message so the
                // later tool_result has a matching tool_use; otherwise Anthropic rejects
                // an orphan tool_result with HTTP 400.
                let placeholder = LanguageModelToolUse {
                    id: id.clone(),
                    name: tool_name.clone(),
                    raw_input: raw_input.clone(),
                    input: serde_json::Value::Null,
                    is_input_complete: true,
                    thought_signature: None,
                };
                self.append_assistant_tool_use(placeholder, cx);
                // Surface the parse failure back to the model as an error tool_result (in a user message).
                let result = LanguageModelToolResult {
                    tool_use_id: id.clone(),
                    tool_name: tool_name.clone(),
                    is_error: true,
                    content: format!(
                        "Tool input JSON parse failed: {json_parse_error}\nraw: {raw_input}"
                    ),
                };
                self.push_tool_result(result, cx);
                cx.emit(ThreadEvent::ToolResult {
                    id,
                    output: json_parse_error,
                    is_error: true,
                });
            }
            Err(e) => {
                cx.emit(ThreadEvent::Error(e));
            }
        }
    }

    fn append_tool_result(
        this: &gpui::WeakEntity<Self>,
        tu: LanguageModelToolUse,
        output: String,
        is_error: bool,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        this.update(cx, |this, cx| {
            let result = LanguageModelToolResult {
                tool_use_id: tu.id.clone(),
                tool_name: tu.name.clone(),
                is_error,
                content: output,
            };
            this.push_tool_result(result, cx);
        })?;
        Ok(())
    }

    /// Append a tool_result to a user message. Per the Anthropic wire contract,
    /// tool_results must live in a user-role message paired with the preceding
    /// assistant turn's tool_use. Multiple consecutive tool_results accumulate
    /// into the same user message.
    fn push_tool_result(&mut self, result: LanguageModelToolResult, cx: &mut Context<Self>) {
        let needs_new = match self.messages.last() {
            Some(m) => m.role != Role::User,
            None => true,
        };
        if needs_new {
            self.messages.push(Message::user_with_content(Vec::new()));
        }
        if let Some(m) = self.messages.last_mut() {
            m.push_content(MessageContent::ToolResult(result));
        }
        let _ = cx;
    }

    fn append_assistant_text(&mut self, text: String, _cx: &mut Context<Self>) {
        let needs_new = match self.messages.last() {
            Some(m) => m.role != Role::Assistant,
            None => true,
        };
        if needs_new {
            self.messages.push(Message::assistant(Vec::new()));
        }
        if let Some(m) = self.messages.last_mut() {
            match m.content.last_mut() {
                Some(MessageContent::Text(existing)) => existing.push_str(&text),
                _ => m.push_text(text),
            }
        }
    }

    /// Accumulate thinking deltas into the current assistant message: consecutive
    /// ThinkingDelta merge into one thinking block (the first non-empty signature
    /// is retained, since Anthropic requires a signature when echoing thinking back).
    fn append_assistant_thinking(
        &mut self,
        text: String,
        signature: Option<String>,
        _cx: &mut Context<Self>,
    ) {
        let needs_new = match self.messages.last() {
            Some(m) => m.role != Role::Assistant,
            None => true,
        };
        if needs_new {
            self.messages.push(Message::assistant(Vec::new()));
        }
        if let Some(m) = self.messages.last_mut() {
            match m.content.last_mut() {
                Some(MessageContent::Thinking {
                    text: existing,
                    signature: sig,
                }) => {
                    existing.push_str(&text);
                    if sig.is_none() {
                        *sig = signature;
                    }
                }
                _ => m.push_content(MessageContent::Thinking { text, signature }),
            }
        }
    }

    /// Append a tool_use block to the current assistant message (alongside text/thinking, as one turn's output).
    fn append_assistant_tool_use(&mut self, tu: LanguageModelToolUse, _cx: &mut Context<Self>) {
        let needs_new = match self.messages.last() {
            Some(m) => m.role != Role::Assistant,
            None => true,
        };
        if needs_new {
            self.messages.push(Message::assistant(Vec::new()));
        }
        if let Some(m) = self.messages.last_mut() {
            m.push_content(MessageContent::ToolUse(tu));
        }
    }

    fn finalize_assistant_message(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = self.messages.last_mut()
            && m.role == Role::Assistant
            && m.content.is_empty()
        {
            m.push_text(String::new());
        }
        cx.notify();
    }

    /// Backfill a trailing unpaired tool_use: if the last assistant message holds a
    /// tool_use block with no matching tool_result (after a cancelled turn or a
    /// crash-recovery reload), synthesize an error tool_result into a user message.
    /// Otherwise Anthropic rejects the dangling assistant tool_use with HTTP 400,
    /// freezing the conversation.
    fn reconcile_tool_uses(&mut self, cx: &mut Context<Self>) {
        let orphans: Vec<(String, std::sync::Arc<str>)> = match self.messages.last() {
            Some(m) if m.role == Role::Assistant => {
                let paired: std::collections::HashSet<&str> = self
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter_map(|c| match c {
                        MessageContent::ToolResult(tr) => Some(tr.tool_use_id.as_str()),
                        _ => None,
                    })
                    .collect();
                m.content
                    .iter()
                    .filter_map(|c| match c {
                        MessageContent::ToolUse(tu) if !paired.contains(tu.id.as_str()) => {
                            Some((tu.id.clone(), tu.name.clone()))
                        }
                        _ => None,
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        if orphans.is_empty() {
            return;
        }
        let content: Vec<MessageContent> = orphans
            .into_iter()
            .map(|(id, name)| {
                MessageContent::ToolResult(LanguageModelToolResult {
                    tool_use_id: id,
                    tool_name: name,
                    is_error: true,
                    content: "Tool not executed (session interrupted or cancelled)".to_string(),
                })
            })
            .collect();
        self.messages.push(Message::user_with_content(content));
        cx.notify();
    }

    /// Map `messages` into a `LanguageModelRequest` (including tool definitions).
    /// A sub-agent's system prompt, when set, is prepended as a `System` message;
    /// the Anthropic wire mapper lifts `System` messages into the top-level
    /// `system` field, and other wires treat it as a leading message. The main
    /// thread has no `system` field, so a runtime-identity prompt (cwd,
    /// project, os, shell, date) is minted here instead — see
    /// `system_prompt::build_main_system_prompt`. Thread id is deliberately
    /// absent; the model fetches it via the `self_info` tool.
    fn build_completion_request(&self) -> LanguageModelRequest {
        // TODO(prefix-cache): once per-turn tool-result truncation / image
        // stripping / history rewriting lands, route the request through
        // `prefix_stability::AppendOnlyContextManager` so the byte-stable
        // prefix is preserved up to the divergence point. manox's
        // `Thread::messages` is append-only today, so the prefix is naturally
        // stable and no stabilization pass is needed yet — but introducing any
        // rewrite without that layer would silently break the provider's
        // prefix cache.
        let mut messages: Vec<LanguageModelRequestMessage> = Vec::new();
        let mut system = self.system.clone().unwrap_or_else(|| {
            crate::system_prompt::build_main_system_prompt(
                &self.cwd,
                self.project.as_deref(),
                self.approval_mode,
            )
        });
        // Sub-agents carry their own system prompt from `agents/*.md`; the main
        // thread already has the language directive baked in via
        // `build_main_system_prompt`. Append it for sub-agents so their reply
        // language follows the UI locale too — the prompt prose itself stays
        // English, only this one directive varies.
        if self.system.is_some() {
            system.push_str(crate::system_prompt::language_directive());
        }
        // In plan mode, append the read-only-constraint addendum so the model
        // knows it must submit a plan via `exit_plan_mode` rather than act.
        if self.plan_mode {
            system.push_str(crate::system_prompt::PLAN_MODE_ADDENDUM);
        }
        // The system prompt is the head of the cached prefix. `cache_control`
        // breakpoints are placed by `provider::anthropic_cache::apply_prompt_caching`
        // (single source of truth); the `cache` flag is advisory metadata, not
        // read by the wire mapper today — kept aligned with that intent.
        messages.push(LanguageModelRequestMessage {
            role: Role::System,
            content: vec![MessageContent::Text(system)],
            cache: true,
        });
        // Map canonical messages to the request, stripping the `agent` tool's
        // JSON envelope to just its `final` text. The full sub-conversation
        // stays in `self.messages` for persistence and UI rebuild, but the
        // parent model must only see the final reply — otherwise every sub-agent
        // tool call, tool result, and reasoning block leaks into the parent's
        // context, defeating the point of spawning an isolated sub-agent.
        //
        // The `cache` flag marks the trailing two user/assistant messages as
        // cache-anchor candidates. It is advisory metadata today — the actual
        // `cache_control` breakpoints are placed by `apply_prompt_caching`
        // against messages[-2]/messages[-1] — but keeping the flag aligned with
        // that intent documents the contract for a future wire mapper that
        // reads it.
        let mapped: Vec<LanguageModelRequestMessage> = self
            .messages
            .iter()
            .map(|m| LanguageModelRequestMessage {
                role: m.role,
                content: m.content.iter().map(model_facing_content).collect(),
                cache: false,
            })
            .collect();
        let len = mapped.len();
        for (i, mut m) in mapped.into_iter().enumerate() {
            if i + 2 >= len {
                m.cache = true;
            }
            messages.push(m);
        }
        let tools = if self.plan_mode {
            // In plan mode, advertise read-only tools plus the synthesized
            // `exit_plan_mode` tool (not in the registry). The `agent` tool is
            // read-only (see `SpawnAgentTool::is_read_only`), so it is included
            // automatically — letting the main thread delegate research to the
            // bundled `plan`/`explore` sub-agents with isolated context. Write
            // tools and `bash` stay hidden; `run_tool_inner` backstops any
            // stray write call. Plan mode takes precedence over
            // `turn_tool_filter` — it is the stricter, user-visible safety
            // contract.
            let mut list = self.tools.to_request_tools_read_only();
            list.push(exit_plan_mode_request_tool());
            list
        } else {
            match self.turn_tool_filter.as_deref() {
                Some(f) if !f.is_empty() => self.tools.to_request_tools_filtered(f),
                _ => self.tools.to_request_tools(),
            }
        };
        LanguageModelRequest {
            messages,
            tools,
            ..Default::default()
        }
    }
}

/// Strip the `agent` tool's JSON envelope from a ToolResult so only its `final`
/// text reaches the model. The canonical `Thread::messages` keep the full
/// envelope (for persistence and UI rebuild); this mapping is applied only when
/// building a request, so the sub-conversation never leaks into the parent's
/// context. Non-`agent` content passes through unchanged.
fn model_facing_content(c: &MessageContent) -> MessageContent {
    match c {
        MessageContent::ToolResult(tr) if tr.tool_name.as_ref() == "agent" => {
            MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: tr.tool_use_id.clone(),
                tool_name: tr.tool_name.clone(),
                is_error: tr.is_error,
                content: crate::tools::agent::agent_final_text(&tr.content),
            })
        }
        other => other.clone(),
    }
}

/// Truncate a summary to `max_chars` (appending an ellipsis when cut) and collapse it to a single line.
pub(crate) fn truncate_summary(s: &str, max_chars: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > max_chars {
        let t: String = one_line.chars().take(max_chars).collect();
        format!("{t}…")
    } else {
        one_line
    }
}

/// Whether a message has any non-empty `Text` content block.
pub(crate) fn message_has_text(m: &Message) -> bool {
    m.content
        .iter()
        .any(|c| matches!(c, MessageContent::Text(t) if !t.trim().is_empty()))
}

/// Build a human-readable title for a tool call.
pub fn tool_title(name: &str, input: &serde_json::Value) -> String {
    match name {
        "read_file" | "write_file" | "list_directory" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            format!("{name} {path}")
        }
        // edit_file's input is a single `patch` string whose first `[PATH#TAG]`
        // header names the target file. The path is everything before the last
        // `#`, so paths containing `#` survive.
        "edit_file" => {
            let patch = input.get("patch").and_then(|v| v.as_str()).unwrap_or("");
            let path = patch
                .lines()
                .find_map(|l| {
                    let l = l.trim();
                    let inner = l.strip_prefix('[')?.strip_suffix(']')?;
                    Some(inner.rsplit_once('#')?.0.to_string())
                })
                .unwrap_or_default();
            format!("edit_file {path}")
        }
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let single = cmd.lines().next().unwrap_or("").trim().to_string();
            if single.chars().count() > 80 {
                let t: String = single.chars().take(80).collect();
                format!("{t}…")
            } else {
                single
            }
        }
        "grep" => {
            let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("grep {p}")
        }
        "glob" => {
            let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("glob {p}")
        }
        "agent" => {
            let st = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            let trimmed = if prompt.chars().count() > 60 {
                let t: String = prompt.chars().take(60).collect();
                format!("{t}…")
            } else {
                prompt.to_string()
            };
            format!("agent: {st} — {trimmed}")
        }
        "AskUserQuestion" => {
            let q = input
                .get("questions")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|q| q.get("question"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let single = q.lines().next().unwrap_or("").trim();
            let trimmed = if single.chars().count() > 80 {
                let t: String = single.chars().take(80).collect();
                format!("{t}…")
            } else {
                single.to_string()
            };
            if trimmed.is_empty() {
                "AskUserQuestion".to_string()
            } else {
                format!("AskUserQuestion: {trimmed}")
            }
        }
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{model_facing_content, tool_title};
    use crate::language_model::{
        AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
        LanguageModelToolResult, MessageContent,
    };
    use crate::message::Message;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn edit_file_title_extracts_path_not_tag() {
        // The `[PATH#TAG]` header must surface the path, not the 4-hex tag.
        let input = json!({ "patch": "[src/main.rs#1A2B]\nSWAP 5.=5:\n+X" });
        assert_eq!(tool_title("edit_file", &input), "edit_file src/main.rs");
    }

    #[test]
    fn edit_file_title_path_with_hash_survives() {
        // A path containing `#` keeps everything before the LAST `#`.
        let input = json!({ "patch": "[a/b#issue.rs#FF0E]\nDEL 1" });
        assert_eq!(tool_title("edit_file", &input), "edit_file a/b#issue.rs");
    }

    #[test]
    fn edit_file_title_missing_header_is_empty() {
        let input = json!({ "patch": "SWAP 5.=5:\n+X" });
        assert_eq!(tool_title("edit_file", &input), "edit_file ");
    }

    #[test]
    fn ask_user_question_title_uses_first_question() {
        let input = json!({
            "questions": [
                { "question": "Which framework?", "header": "Framework",
                  "options": [{"label":"A","description":"a"}], "multiSelect": false }
            ]
        });
        assert_eq!(
            tool_title("AskUserQuestion", &input),
            "AskUserQuestion: Which framework?"
        );
    }

    #[test]
    fn ask_user_question_title_falls_back_without_questions() {
        assert_eq!(tool_title("AskUserQuestion", &json!({})), "AskUserQuestion");
    }

    /// Bash tool title is the first line of the command, no `bash:` prefix —
    /// the card already carries a terminal icon, so the prefix would be
    /// redundant. Truncates to 80 chars with a trailing ellipsis.
    #[test]
    fn bash_title_strips_prefix_and_uses_first_line() {
        let input = json!({ "command": "gh pr create -R dspo/manox --title foo" });
        assert_eq!(
            tool_title("bash", &input),
            "gh pr create -R dspo/manox --title foo"
        );

        let long = "a".repeat(120);
        let input = json!({ "command": long.clone() });
        let out = tool_title("bash", &input);
        assert_eq!(out.chars().count(), 81);
        assert!(out.ends_with('…'));

        let multi = "git status\ncargo build --release";
        let input = json!({ "command": multi });
        assert_eq!(tool_title("bash", &input), "git status");

        let input = json!({});
        assert_eq!(tool_title("bash", &input), "");
    }

    /// The `agent` tool's persisted ToolResult carries a JSON envelope
    /// `{"final":..., "messages":[...]}`. The model must only see the `final`
    /// text — otherwise the sub-agent's whole conversation (tool calls, results,
    /// reasoning) leaks into the parent's context. `model_facing_content`
    /// strips the envelope; the canonical `Thread::messages` keep it.
    #[test]
    fn model_facing_content_strips_agent_envelope_to_final() {
        let sub = vec![Message::user("research foo".to_string())];
        let envelope = json!({ "final": "found 3 files", "messages": sub }).to_string();
        let tr = MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: "tu_1".to_string(),
            tool_name: Arc::from("agent"),
            is_error: false,
            content: envelope,
        });
        let stripped = model_facing_content(&tr);
        let MessageContent::ToolResult(out) = stripped else {
            panic!("expected ToolResult");
        };
        assert_eq!(out.content, "found 3 files");
        assert_eq!(out.tool_name.as_ref(), "agent");
        // Original canonical content is untouched (still the envelope).
        let MessageContent::ToolResult(orig) = tr else {
            unreachable!()
        };
        assert!(orig.content.contains("\"messages\""));
    }

    /// A non-`agent` tool result passes through unchanged — no envelope
    /// stripping, no content mutation.
    #[test]
    fn model_facing_content_passes_non_agent_through() {
        let tr = MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: "tu_2".to_string(),
            tool_name: Arc::from("bash"),
            is_error: false,
            content: "command output".to_string(),
        });
        let MessageContent::ToolResult(out) = model_facing_content(&tr) else {
            panic!("expected ToolResult");
        };
        assert_eq!(out.content, "command output");
        assert_eq!(out.tool_name.as_ref(), "bash");
    }

    /// A legacy `agent` result (plain text, no envelope) falls back to the raw
    /// content rather than emptying it.
    #[test]
    fn model_facing_content_agent_legacy_falls_back() {
        let tr = MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: "tu_3".to_string(),
            tool_name: Arc::from("agent"),
            is_error: false,
            content: "plain summary".to_string(),
        });
        let MessageContent::ToolResult(out) = model_facing_content(&tr) else {
            panic!("expected ToolResult");
        };
        assert_eq!(out.content, "plain summary");
    }

    /// True regression guard for the `run_tool_inner` double-lease fix: it
    /// calls `run_tool_inner` itself with a `self_info` tool use. On the
    /// unfixed code (`this.update` wrapping the tool call) the owning
    /// Thread holds a write lease while `self_info`'s `run` does
    /// `read_with` on it — tripping `double_lease_panic` mid-task, so the
    /// spawned task never completes and `result` stays `None`. On the fixed
    /// code (`cx.update`, no entity lease) the task completes, returns
    /// `Ok(())`, and appends a `ToolResult` carrying the thread id.
    #[test]
    fn run_tool_inner_self_info_does_not_double_lease() {
        use crate::language_model::{LanguageModelToolResult, LanguageModelToolUse};
        use std::sync::{Arc, Mutex};
        use tokio_util::sync::CancellationToken;

        crate::agent_def::init();

        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-run-tool-inner", "/tmp", Vec::new()),
                None,
                cx,
            )
        });
        let tu = LanguageModelToolUse {
            id: "tu_1".to_string(),
            name: Arc::from("self_info"),
            raw_input: "{}".to_string(),
            input: serde_json::json!({}),
            is_input_complete: true,
            thought_signature: None,
        };
        let weak = thread.downgrade();
        let cancel = CancellationToken::new();
        let result: Arc<Mutex<Option<anyhow::Result<()>>>> = Arc::new(Mutex::new(None));
        let r = result.clone();
        cx.spawn(|cx| {
            let mut cx = cx.clone();
            async move {
                *r.lock().unwrap() =
                    Some(super::Thread::run_tool_inner(weak, tu, cancel, &mut cx).await);
            }
        })
        .detach();
        cx.run_until_parked();

        let res =
            result.lock().unwrap().take().expect(
                "run_tool_inner did not complete (task panicked — double-lease regression?)",
            );
        assert!(res.is_ok(), "run_tool_inner failed: {:?}", res.err());

        let messages = cx.update(|cx| thread.read_with(cx, |t, _| t.messages.clone()));
        let last = messages.last().expect("no message appended");
        let MessageContent::ToolResult(LanguageModelToolResult { content, .. }) =
            last.content.last().expect("no content")
        else {
            panic!("expected ToolResult, got {:?}", last.content);
        };
        assert!(
            content.contains("reg-run-tool-inner"),
            "expected thread id in tool result, got: {content}"
        );
    }

    /// Regression: `cancel()` must clear `pending_plan_approval` immediately
    /// so the oneshot sender is dropped. Without the fix, the UI clears its
    /// `pending_plan` overlay (via the `Stop` event) but the thread's oneshot
    /// lingers — a late `respond_plan_approval` silently no-ops and the async
    /// `run_plan_approval` task stays parked until the entity is dropped.
    #[test]
    fn cancel_clears_pending_plan_approval() {
        use crate::tool::PlanApprovalResponse;

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-cancel-plan", "/tmp", Vec::new()),
                None,
                cx,
            )
        });

        // Simulate a running turn with a pending plan approval.
        cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.plan_mode = true;
                let cancel = tokio_util::sync::CancellationToken::new();
                t.turn_cancel = Some(cancel);
                let (tx, _rx) = tokio::sync::oneshot::channel::<PlanApprovalResponse>();
                t.pending_plan_approval.insert("plan-1".to_string(), tx);
            });
        });

        // Cancel the turn.
        cx.update(|cx| {
            thread.update(cx, |t, cx| t.cancel(cx));
        });

        // Both turn_cancel and pending_plan_approval must be cleared.
        cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                assert!(t.turn_cancel.is_none(), "cancel should take turn_cancel");
                assert!(
                    t.pending_plan_approval.is_empty(),
                    "cancel should clear pending_plan_approval (oneshot sender \
                     still lingering — UI cleared but thread didn't)"
                );
                // plan_mode is NOT cleared by cancel — it stays true so the
                // next turn still builds a plan-mode request.
                assert!(t.plan_mode, "cancel should not clear plan_mode");
            });
        });
    }

    /// Regression: `cancel()` must clear `pending_authorizations` symmetrically
    /// with `pending_plan_approval`. The cancellation token resolves in-flight
    /// `select!` arms, but the oneshot senders must also be dropped so a late
    /// `respond_authorization` from the UI silently no-ops instead of landing
    /// on a closed channel.
    #[test]
    fn cancel_clears_pending_authorizations() {
        use crate::tool::ToolAuthorizationResponse;

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-cancel-auth", "/tmp", Vec::new()),
                None,
                cx,
            )
        });

        cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                let cancel = tokio_util::sync::CancellationToken::new();
                t.turn_cancel = Some(cancel);
                let (tx, _rx) = tokio::sync::oneshot::channel::<ToolAuthorizationResponse>();
                t.pending_authorizations.insert("auth-1".to_string(), tx);
                // Simulate a bubbled child authorization pending on the parent.
                t.pending_child_auth.insert(
                    "parent::child".to_string(),
                    super::ChildAuthRoute {
                        child: gpui::WeakEntity::<super::Thread>::new_invalid(),
                        child_auth_id: "child".to_string(),
                    },
                );
            });
        });

        cx.update(|cx| {
            thread.update(cx, |t, cx| t.cancel(cx));
        });

        cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                assert!(t.turn_cancel.is_none(), "cancel should take turn_cancel");
                assert!(
                    t.pending_authorizations.is_empty(),
                    "cancel should clear pending_authorizations (oneshot sender \
                     still lingering — late respond_authorization would hit a \
                     closed channel)"
                );
                assert!(
                    t.pending_child_auth.is_empty(),
                    "cancel should clear pending_child_auth (orphaned route \
                     still lingering — late composite-id response would \
                     traverse to child before no-oping)"
                );
            });
        });
    }

    /// Regression: `run_plan_approval` Approve branch must append the
    /// ToolResult to the message list BEFORE clearing `plan_mode`. The old
    /// code set `plan_mode = false` first; if `append_tool_result` then
    /// failed (entity gone, infra error), the thread would exit plan mode
    /// without recording the approval — inconsistent state for the next
    /// `build_completion_request`.
    #[test]
    fn plan_approval_approve_appends_result_before_clearing_plan_mode() {
        use crate::language_model::LanguageModelToolUse;
        use std::sync::{Arc, Mutex};
        use tokio_util::sync::CancellationToken;

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-plan-approve-order", "/tmp", Vec::new()),
                None,
                cx,
            )
        });

        let tu = LanguageModelToolUse {
            id: "tu_plan_1".to_string(),
            name: Arc::from("exit_plan_mode"),
            raw_input: r#"{"plan":"do stuff"}"#.to_string(),
            input: serde_json::json!({"plan": "do stuff"}),
            is_input_complete: true,
            thought_signature: None,
        };

        // Enable plan mode; `run_plan_approval` creates its own oneshot
        // and stores the sender in `pending_plan_approval`.
        cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.plan_mode = true;
            });
        });

        let weak = thread.downgrade();
        let cancel = CancellationToken::new();
        let result: Arc<Mutex<Option<anyhow::Result<()>>>> = Arc::new(Mutex::new(None));
        let r = result.clone();

        // Spawn run_plan_approval.
        cx.spawn(|cx| {
            let mut cx = cx.clone();
            async move {
                *r.lock().unwrap() =
                    Some(super::Thread::run_plan_approval(weak, tu, cancel, &mut cx).await);
            }
        })
        .detach();

        // Let the task reach the tokio::select! and park on the oneshot.
        cx.run_until_parked();

        // Send the approval via `respond_plan_approval`, which extracts the
        // sender from `pending_plan_approval` and resolves the oneshot.
        cx.update(|cx| {
            thread.update(cx, |t, cx| {
                t.respond_plan_approval(
                    "tu_plan_1",
                    crate::tool::PlanApprovalResponse::Approve,
                    cx,
                );
            });
        });

        // Let the task process the approval and complete.
        cx.run_until_parked();

        let res = result
            .lock()
            .unwrap()
            .take()
            .expect("run_plan_approval did not complete");
        assert!(res.is_ok(), "run_plan_approval failed: {:?}", res.err());

        // Verify: plan_mode is now false AND the last message is the
        // approval ToolResult. The ordering guarantee is that the
        // ToolResult was appended before plan_mode was cleared.
        cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                assert!(!t.plan_mode, "plan_mode should be false after approval");
                let last = t.messages.last().expect("no message after approval");
                let tr = last
                    .content
                    .iter()
                    .find_map(|c| match c {
                        MessageContent::ToolResult(tr) => Some(tr),
                        _ => None,
                    })
                    .expect("no ToolResult in last message");
                assert_eq!(tr.tool_use_id.as_str(), "tu_plan_1");
                assert!(
                    tr.content.contains("approved"),
                    "expected approval message in ToolResult, got: {}",
                    tr.content
                );
            });
        });
    }

    /// Regression guard for the YOLO + `AskUserQuestion` interaction.
    /// `AskUserQuestion`'s `run` body is unreachable — its result is built
    /// from the `ToolAuthorizationResponse` at the authorization gate. YOLO
    /// bypasses *permission* gates, but must not bypass this tool, or the
    /// model would hit the unreachable `run` and get an error string instead
    /// of the user's answers. Under the fix, YOLO + `AskUserQuestion` still
    /// enters the gate; a cancelled turn resolves the gate to `Deny`, so the
    /// tool result is the user-refusal message — not the `run`-body error.
    #[test]
    fn run_tool_inner_yolo_ask_user_question_hits_gate_not_run() {
        use crate::language_model::{LanguageModelToolResult, LanguageModelToolUse};
        use std::sync::{Arc, Mutex};
        use tokio_util::sync::CancellationToken;

        crate::agent_def::init();

        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            let mut rec =
                crate::db::ThreadRecord::for_test("reg-yolo-ask-user", "/tmp", Vec::new());
            rec.approval_mode = crate::thread::ApprovalMode::Yolo.as_i64();
            super::Thread::restore(rec, None, cx)
        });
        let tu = LanguageModelToolUse {
            id: "tu_aq".to_string(),
            name: Arc::from("AskUserQuestion"),
            raw_input: "{}".to_string(),
            input: serde_json::json!({}),
            is_input_complete: true,
            thought_signature: None,
        };
        let weak = thread.downgrade();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result: Arc<Mutex<Option<anyhow::Result<()>>>> = Arc::new(Mutex::new(None));
        let r = result.clone();
        cx.spawn(|cx| {
            let mut cx = cx.clone();
            async move {
                *r.lock().unwrap() =
                    Some(super::Thread::run_tool_inner(weak, tu, cancel, &mut cx).await);
            }
        })
        .detach();
        cx.run_until_parked();

        let res = result
            .lock()
            .unwrap()
            .take()
            .expect("run_tool_inner did not complete");
        assert!(res.is_ok(), "run_tool_inner failed: {:?}", res.err());

        let messages = cx.update(|cx| thread.read_with(cx, |t, _| t.messages.clone()));
        let last = messages.last().expect("no message appended");
        let MessageContent::ToolResult(LanguageModelToolResult { content, .. }) =
            last.content.last().expect("no content")
        else {
            panic!("expected ToolResult, got {:?}", last.content);
        };
        assert!(
            content.contains("User denied execution"),
            "YOLO + AskUserQuestion should hit the authorization gate (Deny on cancel), \
             got: {content}"
        );
        assert!(
            !content.contains("resolved by the UI"),
            "YOLO must not bypass AskUserQuestion into its unreachable run body, got: {content}"
        );
    }

    /// `accumulate_token_usage` / `finalize_request_usage` accounting is covered
    /// in `token_meter::tests::accumulate_tracks_running_total_and_resets_on_finalize`
    /// (no `Thread`/gpui fixture needed there — the meter is self-contained).
    ///
    /// Live regression guard for the inter-batch yield fix: drives a real
    /// `run_turn` against Bailian glm-5.2[1m] through the full gpui↔tokio
    /// bridge. Before the fix, the yield future returned `Pending` forever,
    /// so the turn hung after the first batch — the assistant message held
    /// only the empty `ContentBlockStart` thinking block and output_tokens
    /// stayed 0. After the fix the turn drains the whole stream and produces
    /// real text. Requires `MANOX_RUN_LIVE=1` + DASHSCOPE_API_KEY.
    #[tokio::test(flavor = "current_thread")]
    async fn live_run_turn_drains_full_stream() {
        if std::env::var("MANOX_RUN_LIVE").is_err() {
            return;
        }
        crate::agent_def::init();
        let config = crate::provider::CxConfig::load_default().expect("load config");
        let registry = crate::provider::registry::ProviderRegistry::from_config(config);
        let model = registry
            .models()
            .iter()
            .find(|m| m.provider_name() == "百炼" && m.name().contains("glm-5.2"))
            .cloned()
            .expect("百炼 glm-5.2[1m] anthropic");

        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                "live-run-turn",
                "/Users/chenzhongrun/projects/dspo/manox",
                vec![Message::user("当前 thread id 是?".into())],
            );
            // Pin a title so `maybe_generate_title` short-circuits — its
            // spawned title-stream task would otherwise outlive the test and
            // trip gpui's leaked-handle check at teardown.
            rec.title = Some("live run-turn".into());
            super::Thread::restore(rec, Some(model), cx)
        });
        cx.update(|cx| {
            thread.update(cx, |t, cx| t.run_turn(cx));
        });

        // Drive the gpui foreground executor while the tokio provider task
        // streams events back over async_channel. The turn is done once
        // `is_running` flips to false. Bound it so a regression (hang) fails
        // fast instead of stalling the test runner.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            cx.run_until_parked();
            let done = cx.update(|cx| thread.read_with(cx, |t, _| !t.is_running()));
            if done {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("run_turn did not finish within 60s — inter-batch yield regression?");
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }

        let msgs = cx.update(|cx| thread.read_with(cx, |t, _| t.messages.clone()));
        // The assistant turn must have produced real text content, not just an
        // empty `ContentBlockStart` thinking block (the pre-fix symptom).
        let assistant_text: String = msgs
            .iter()
            .rev()
            .find(|m| m.role == crate::language_model::Role::Assistant)
            .into_iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|c| match c {
                crate::language_model::MessageContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !assistant_text.is_empty(),
            "assistant produced no text — stream was not fully drained (got messages: {msgs:?})"
        );
        eprintln!(
            "assistant text: {}",
            assistant_text.chars().take(200).collect::<String>()
        );
    }

    /// A `LanguageModel` that replays a fixed event sequence and then closes the
    /// stream — used to drive `run_turn` without a live provider. The sequence
    /// is shared (via `Arc`) so a re-prompted outer loop yields the same events
    /// again. Only `Ok` events are supported, which is enough for the
    /// persistence regressions (the stream-close-without-`Stop` path is the
    /// whole point).
    struct ReplayMockModel {
        id: String,
        events: Arc<Vec<LanguageModelCompletionEvent>>,
    }

    impl crate::language_model::LanguageModel for ReplayMockModel {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn name(&self) -> String {
            self.id.clone()
        }
        fn provider_id(&self) -> String {
            "test".into()
        }
        fn provider_name(&self) -> String {
            "test".into()
        }
        fn wire_api(&self) -> crate::provider::WireApi {
            crate::provider::WireApi::Anthropic
        }
        fn max_token_count(&self) -> u64 {
            4096
        }
        fn stream_completion(
            &self,
            _request: LanguageModelRequest,
            _cx: &gpui::AsyncApp,
        ) -> futures::future::BoxFuture<
            'static,
            anyhow::Result<
                futures::stream::BoxStream<'static, anyhow::Result<LanguageModelCompletionEvent>>,
            >,
        > {
            let events = self.events.clone();
            Box::pin(async move {
                use futures::StreamExt as _;
                let events: Vec<_> = events.iter().cloned().map(Ok).collect();
                Ok(futures::stream::iter(events).boxed())
            })
        }
    }

    /// Regression for the switch-loses-assistant bug: a turn whose stream
    /// produces text but ends WITHOUT a `MessageStop` (provider hiccup,
    /// non-SSE compatibility response) leaves the assistant text only in
    /// memory — no terminal `Stop` event reaches a subscriber, so without the
    /// task-tail save the next thread switch reloads the stale db row and the
    /// assistant content vanishes. The `run_turn` task tail calls
    /// `save_thread` unconditionally for exactly this case.
    #[test]
    fn run_turn_persists_assistant_text_without_stop_event() {
        use crate::db::ThreadsDatabase;
        use std::sync::Arc;

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        // Release the test ThreadStore entity before teardown — the gpui
        // leaked-handle check trips on a process-global entity held alive past
        // `TestAppContext` drop. Drop runs even on panic, so the assertion
        // failure path stays clean too.
        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        // Prime the process-global ThreadStore with an in-memory db so the
        // task-tail save lands somewhere queryable instead of the real
        // `~/.config/cx/manox/threads.db`.
        let db =
            Arc::new(ThreadsDatabase::open(std::path::Path::new(":memory:")).expect("open mem db"));
        let db_handle = db.clone();
        cx.update(|cx| {
            crate::thread_store::init_for_test(db_handle, cx);
        });

        // Stream emits text and then closes with NO `Stop` — the bug scenario.
        let model: AnyLanguageModel = Arc::new(ReplayMockModel {
            id: "test/replay".into(),
            events: Arc::new(vec![LanguageModelCompletionEvent::Text(
                "hello world".into(),
            )]),
        });

        let thread_id = "reg-persist-no-stop";
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                thread_id,
                "/tmp",
                vec![Message::user("当前 thread id 是?".into())],
            );
            // Pin the title so `maybe_generate_title` short-circuits at the
            // `title_last_eval_user_count` gate — otherwise it spawns a title
            // stream task that outlives the test and trips gpui's leaked-handle
            // check at teardown.
            rec.title = Some("regression".into());
            super::Thread::restore(rec, Some(model), cx)
        });

        cx.update(|cx| {
            thread.update(cx, |t, cx| t.run_turn(cx));
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            cx.run_until_parked();
            let done = cx.update(|cx| thread.read_with(cx, |t, _| !t.is_running()));
            if done {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("run_turn did not finish within 30s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // The task-tail save is fire-and-forget on the background executor;
        // `run_until_parked` drains it so the upsert has committed by here.
        cx.run_until_parked();

        let loaded = db.load(thread_id).expect("load db").expect("row present");
        let assistant_text: String = loaded
            .messages
            .iter()
            .filter(|m| m.role == crate::language_model::Role::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|c| match c {
                crate::language_model::MessageContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant_text, "hello world",
            "assistant text was not persisted by the run_turn task-tail save"
        );
        // A successful persist must stamp a non-zero revision so later stale
        // snapshots are rejected by the upsert guard.
        assert!(loaded.revision > 0, "persisted revision must be non-zero");
    }
}
