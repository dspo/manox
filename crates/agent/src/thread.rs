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

use anyhow::Result;
use futures::FutureExt as _;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Task, WeakEntity};
use tokio_util::sync::CancellationToken;

use crate::db::ThreadRecord;
use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, LanguageModelToolResult, LanguageModelToolUse, MessageContent,
    Role, StopReason,
};
use crate::message::Message;
use crate::tool::{PermissionCache, PermissionDecision, ToolAuthorizationResponse, ToolRegistry};
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
    /// A completion turn ended.
    Stop(StopReason),
    /// An error during streaming.
    Error(anyhow::Error),
}

pub struct Thread {
    pub id: ThreadId,
    messages: Vec<Message>,
    model: Option<AnyLanguageModel>,
    tools: Arc<ToolRegistry>,
    permission: Arc<PermissionCache>,
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
    pending_authorizations:
        HashMap<String, tokio::sync::oneshot::Sender<ToolAuthorizationResponse>>,
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
            Self {
                id,
                messages: Vec::new(),
                model: crate::provider::registry::global()
                    .models()
                    .first()
                    .cloned(),
                tools: Arc::new(tools::default_registry(cwd.clone(), weak)),
                permission: Arc::new(PermissionCache::default()),
                cwd,
                project: None,
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
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
            }
        })
    }

    /// Restore a `Thread` from a persisted record (messages + model rebuilt; tools rebuilt from cwd).
    pub fn restore(
        id: ThreadId,
        cwd: PathBuf,
        project: Option<PathBuf>,
        messages: Vec<Message>,
        model: Option<AnyLanguageModel>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let weak = cx.weak_entity();
            Self {
                id,
                messages,
                model,
                tools: Arc::new(tools::default_registry(cwd.clone(), weak)),
                permission: Arc::new(PermissionCache::default()),
                cwd,
                project,
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
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
            }
        })
    }

    /// Construct a sub-agent `Thread` with a restricted tool registry, an
    /// independent permission cache, a system prompt, and a turn cap. The
    /// `tools_fn` closure receives the new thread's own `WeakEntity` so the
    /// `agent` tool (when nesting is allowed) can route back to it.
    #[allow(clippy::too_many_arguments)]
    pub fn new_subagent(
        cwd: PathBuf,
        model: AnyLanguageModel,
        permission: Arc<PermissionCache>,
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
                cwd,
                project: None,
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
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
            }
        })
    }

    /// Build a persistable snapshot (with the first user message as summary). Returns `None` when there is no model (not persisted).
    pub fn snapshot(&self) -> Option<ThreadRecord> {
        let model_id = self.model.as_ref().map(|m| m.id())?;
        Some(ThreadRecord {
            id: self.id.0.clone(),
            summary: self.summary(),
            model_id,
            cwd: self.cwd.display().to_string(),
            project: self
                .project
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            messages: self.messages.clone(),
        })
    }

    /// First user message text, truncated to 60 chars; falls back to the localized default when absent.
    fn summary(&self) -> String {
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
        "(新对话)".to_string()
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

    /// Snapshot of this Thread's always-allow set, for seeding a sub-agent's
    /// permission cache so the child does not re-prompt grants the user already
    /// gave the parent for the same tool.
    pub fn permission_snapshot(&self) -> std::collections::HashSet<String> {
        self.permission.allowed_tools()
    }

    pub fn model(&self) -> Option<&AnyLanguageModel> {
        self.model.as_ref()
    }

    pub fn set_model(&mut self, model: AnyLanguageModel, cx: &mut Context<Self>) {
        self.model = Some(model);
        cx.notify();
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
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
        self.tools = Arc::new(tools::default_registry(dir.clone(), cx.weak_entity()));
        self.project = Some(dir);
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        self.running_turn.is_some()
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
            cx.emit(ThreadEvent::Error(anyhow::anyhow!("未配置模型")));
            return;
        };

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
                cx.notify();
            })
            .ok();
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
        loop {
            let request = this.update(cx, |this, cx| {
                this.pending_tool_uses.clear();
                this.reconcile_tool_uses(cx);
                this.build_completion_request()
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

            let (tool_uses, free_tus, approval_tus) = this.update(cx, |this, _cx| {
                let tool_uses = std::mem::take(&mut this.pending_tool_uses);
                let mut free = Vec::new();
                let mut appr = Vec::new();
                for tu in &tool_uses {
                    let needs_approval = this
                        .tools
                        .get(tu.name.as_ref())
                        .map(|t| t.requires_approval(&tu.input))
                        .unwrap_or(false)
                        && !this.permission.is_always_allowed(tu.name.as_ref());
                    if needs_approval {
                        appr.push(tu.clone());
                    } else {
                        free.push(tu.clone());
                    }
                }
                (tool_uses, free, appr)
            })?;
            if tool_uses.is_empty() {
                break;
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

            // Sub-agent turn cap: stop runaway sub-agents after `max_turns` round-trips.
            // The first hit injects one summary turn so the sub-agent can wrap up
            // with a coherent final message instead of ending mid-work; a second
            // hit (the summary turn itself overflowed) hard-stops.
            let hit_cap = this.update(cx, |this, cx| {
                this.turn_count += 1;
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

        let tool = this.read_with(cx, |this, _| this.tools.get(&name).cloned())?;

        let Some(tool) = tool else {
            let msg = format!("未知工具: {name}");
            Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
            Self::append_tool_result(&this, tu, msg.clone(), true, cx)?;
            return Ok(());
        };

        let needs_approval = tool.requires_approval(&tu.input)
            && !this.read_with(cx, |this, _| this.permission.is_always_allowed(&name))?;
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
            })?;
            match response {
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny) => {
                    let msg = "用户拒绝执行".to_string();
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
        // Invoke the tool via `cx.update` (App context, no entity lease) rather
        // than `this.update`. `this.update` would hold a write lease on the
        // owning Thread, and tools that read the owning Thread from inside
        // their `run` — `self_info` does `self.thread.read_with` — would
        // re-lease the same entity and trip gpui's `double_lease_panic`. The
        // tool returns its `Task` synchronously and does not need the Thread
        // leased on its behalf.
        let result_task: Task<Result<String, String>> =
            cx.update(|cx| tool.run_streaming(input, cancel.clone(), sink, cx));
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
        let msg = "工具未执行（会话被取消）".to_string();
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
            Ok(LanguageModelCompletionEvent::UsageUpdate(_)) => {}
            Ok(LanguageModelCompletionEvent::Stop(reason)) => {
                self.finalize_assistant_message(cx);
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
                        "工具输入 JSON 解析失败: {json_parse_error}\nraw: {raw_input}"
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
            self.messages.push(Message {
                role: Role::User,
                content: Vec::new(),
            });
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
                    content: "工具未执行（会话中断或被取消）".to_string(),
                })
            })
            .collect();
        self.messages.push(Message {
            role: Role::User,
            content,
        });
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
        let mut messages: Vec<LanguageModelRequestMessage> = Vec::new();
        let system = self.system.clone().unwrap_or_else(|| {
            crate::system_prompt::build_main_system_prompt(&self.cwd, self.project.as_deref())
        });
        messages.push(LanguageModelRequestMessage {
            role: Role::System,
            content: vec![MessageContent::Text(system)],
            // The `cache` flag is currently advisory only — the Anthropic wire
            // mapper (`provider/anthropic.rs::content_to_anthropic`) does not yet
            // emit `cache_control` breakpoints. Kept `false` to avoid implying
            // caching that isn't actually wired up.
            cache: false,
        });
        // Map canonical messages to the request, stripping the `agent` tool's
        // JSON envelope to just its `final` text. The full sub-conversation
        // stays in `self.messages` for persistence and UI rebuild, but the
        // parent model must only see the final reply — otherwise every sub-agent
        // tool call, tool result, and reasoning block leaks into the parent's
        // context, defeating the point of spawning an isolated sub-agent.
        messages.extend(self.messages.iter().map(|m| LanguageModelRequestMessage {
            role: m.role,
            content: m.content.iter().map(model_facing_content).collect(),
            cache: false,
        }));
        let tools = match self.turn_tool_filter.as_deref() {
            Some(f) if !f.is_empty() => self.tools.to_request_tools_filtered(f),
            _ => self.tools.to_request_tools(),
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
fn truncate_summary(s: &str, max_chars: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > max_chars {
        let t: String = one_line.chars().take(max_chars).collect();
        format!("{t}…")
    } else {
        one_line
    }
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
    use crate::language_model::{LanguageModelToolResult, MessageContent};
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
                super::ThreadId("reg-run-tool-inner".to_string()),
                std::path::PathBuf::from("/tmp"),
                None,
                Vec::new(),
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
}
