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
//! `PermissionDecision`, which the task `await`s on the gpui executor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
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
use crate::tool::{PermissionCache, PermissionDecision, ToolRegistry};
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
    /// Request user authorization for a tool call. The UI resolves the prompt and calls `Thread::respond_authorization` with the decision.
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
    /// tool_uses collected during the current turn, processed after the stream ends.
    pending_tool_uses: Vec<LanguageModelToolUse>,
    /// Pending authorizations for THIS thread's own tool calls, keyed by
    /// tool_use id. A map (not a single slot) so parallel tool calls can each
    /// await their own decision without overwriting one another.
    pending_authorizations: HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>,
    /// Authorization requests bubbled up from a sub-agent, keyed by a composite
    /// id `<parent_tool_use_id>::<child_auth_id>`. Routing a decision back
    /// forwards it to the owning child thread.
    pending_child_auth: HashMap<String, ChildAuthRoute>,
    /// Sub-agent conversation snapshots keyed by the parent-side tool_use id.
    /// Filled when a sub-agent finishes; the UI reads them to render the
    /// expandable sub-conversation panel.
    subagent_snapshots: HashMap<String, Vec<Message>>,
    /// Sub-agent system prompt; `None` for the main thread (no system prompt injected).
    system: Option<String>,
    /// Nesting depth. Main thread = 0; a sub-agent = parent depth + 1.
    depth: u32,
    /// Max agentic turns before a sub-agent is force-stopped. `None` = unlimited.
    max_turns: Option<u32>,
    /// Completed round-trips in the current turn, for `max_turns` enforcement.
    turn_count: u32,
    /// The running turn task; dropping it aborts the turn.
    running_turn: Option<Task<()>>,
    /// Cancellation token for the running turn. Cancelled by `cancel()` so the
    /// in-flight tool (e.g. `bash`) can reap its process group promptly instead
    /// of relying on task-drop, which does not reach the detached tokio child.
    turn_cancel: Option<CancellationToken>,
}

/// A forwarded authorization request from a sub-agent: which child thread holds
/// the pending decision and what id the child knows it by.
struct ChildAuthRoute {
    child: WeakEntity<Thread>,
    child_auth_id: String,
}

impl EventEmitter<ThreadEvent> for Thread {}

impl Thread {
    /// Construct a new `Thread`, defaulting to the registry's first model and registering the 7 built-in tools plus the `agent` tool.
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
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
                pending_child_auth: HashMap::new(),
                subagent_snapshots: HashMap::new(),
                system: None,
                depth: 0,
                max_turns: None,
                turn_count: 0,
                running_turn: None,
                turn_cancel: None,
            }
        })
    }

    /// Restore a `Thread` from a persisted record (messages + model rebuilt; tools rebuilt from cwd).
    pub fn restore(
        id: ThreadId,
        cwd: PathBuf,
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
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
                pending_child_auth: HashMap::new(),
                subagent_snapshots: HashMap::new(),
                system: None,
                depth: 0,
                max_turns: None,
                turn_count: 0,
                running_turn: None,
                turn_cancel: None,
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
                pending_tool_uses: Vec::new(),
                pending_authorizations: HashMap::new(),
                pending_child_auth: HashMap::new(),
                subagent_snapshots: HashMap::new(),
                system: Some(system),
                depth,
                max_turns: Some(max_turns),
                turn_count: 0,
                running_turn: None,
                turn_cancel: None,
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

    /// Called after the UI resolves authorization: route the decision to the
    /// matching pending responder by id. Handles three cases:
    /// 1. an id matching this thread's own `pending_authorizations` → send to the tool;
    /// 2. a composite id `<parent_tool_use_id>::<child_auth_id>` registered via
    ///    `register_child_auth` → forward to the child thread;
    /// 3. no match → silently drop (stale, cancelled, or already resolved).
    pub fn respond_authorization(
        &mut self,
        id: &str,
        decision: PermissionDecision,
        cx: &mut Context<Self>,
    ) {
        if let Some(tx) = self.pending_authorizations.remove(id) {
            let _ = tx.send(decision);
            return;
        }
        if let Some(route) = self.pending_child_auth.remove(id) {
            if let Some(child) = route.child.upgrade() {
                let child_id = route.child_auth_id.clone();
                child.update(cx, |c, cx| {
                    c.respond_authorization(&child_id, decision, cx);
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

    /// Store a finished sub-agent's full message list keyed by the parent-side
    /// tool_use id, so the UI can render the expandable sub-conversation panel.
    pub fn insert_subagent_snapshot(
        &mut self,
        tool_use_id: String,
        messages: Vec<Message>,
        cx: &mut Context<Self>,
    ) {
        self.subagent_snapshots.insert(tool_use_id, messages);
        cx.notify();
    }

    /// Read-only access to sub-agent snapshots (for UI rendering).
    pub fn subagent_snapshots(&self) -> &HashMap<String, Vec<Message>> {
        &self.subagent_snapshots
    }

    pub fn depth(&self) -> u32 {
        self.depth
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
                let event = tokio::select! {
                    ev = stream.next() => match ev {
                        Some(e) => e,
                        None => break,
                    },
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    }
                };
                let is_stop = matches!(event, Ok(LanguageModelCompletionEvent::Stop(_)));
                this.update(cx, |this, cx| {
                    this.handle_completion_event(event, cx);
                })?;
                if is_stop {
                    break;
                }
            }
            if cancelled {
                break;
            }

            let tool_uses =
                this.update(cx, |this, _cx| std::mem::take(&mut this.pending_tool_uses))?;
            if tool_uses.is_empty() {
                break;
            }

            // Spawn every collected tool_use concurrently on its own gpui task,
            // then await them in order. gpui `Task`s are `!Send` so `tokio::join!`
            // is unavailable, but spawning first lets the tools run in parallel
            // while the ordered `await`s only serialize completion collection.
            // Tool results carry their own `tool_use_id`, so append order is
            // irrelevant to the wire contract.
            let tasks: Vec<Task<Result<()>>> = this.update(cx, |_this, cx| {
                tool_uses
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
            // Drop the owned copy; tasks own their `tu` clones now.
            drop(tool_uses);

            let mut first_err: Option<anyhow::Error> = None;
            for task in tasks {
                if let Err(e) = task.await
                    && first_err.is_none()
                {
                    first_err = Some(e);
                }
                if cancel.is_cancelled() {
                    break;
                }
            }
            if let Some(e) = first_err {
                return Err(e);
            }
            if cancel.is_cancelled() {
                break;
            }

            // Sub-agent turn cap: stop runaway sub-agents after `max_turns` round-trips.
            let hit_cap = this.update(cx, |this, cx| {
                this.turn_count += 1;
                let capped = this
                    .max_turns
                    .map(|m| this.turn_count >= m)
                    .unwrap_or(false);
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

        let needs_approval = tool.requires_approval()
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

            let decision = tokio::select! {
                d = rx => d.unwrap_or(PermissionDecision::Deny),
                _ = cancel.cancelled() => PermissionDecision::Deny,
            };
            // The pending responder is spent whether the UI answered or cancel
            // fired; remove it so a late `respond_authorization` cannot revive a
            // cancelled turn.
            this.update(cx, |this, _cx| {
                this.pending_authorizations.remove(&id);
            })?;
            match decision {
                PermissionDecision::Deny => {
                    let msg = "用户拒绝执行".to_string();
                    Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
                    Self::append_tool_result(&this, tu, msg, true, cx)?;
                    return Ok(());
                }
                PermissionDecision::AlwaysAllow => {
                    this.read_with(cx, |this, _| this.permission.set_always_allowed(&name))?;
                }
                PermissionDecision::AllowOnce => {}
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

        let input = tu.input.clone();
        let (sink, rx) = crate::tool::ToolOutputSink::channel(id.clone().into());
        let id_for_drain = id.clone();
        let result_task: Task<Result<String, String>> = this.update(cx, |_this, cx| {
            tool.run_streaming(input, cancel.clone(), sink, cx)
        })?;
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
        Self::append_tool_result(&this, tu, output_str, is_error, cx)?;
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
    /// `system` field, and other wires treat it as a leading message.
    fn build_completion_request(&self) -> LanguageModelRequest {
        let mut messages: Vec<LanguageModelRequestMessage> = Vec::new();
        if let Some(sys) = &self.system {
            messages.push(LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(sys.clone())],
                cache: false,
            });
        }
        messages.extend(self.messages.iter().map(|m| LanguageModelRequestMessage {
            role: m.role,
            content: m.content.clone(),
            cache: false,
        }));
        LanguageModelRequest {
            messages,
            tools: self.tools.to_request_tools(),
            ..Default::default()
        }
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
            let trimmed = if single.chars().count() > 80 {
                let t: String = single.chars().take(80).collect();
                format!("{t}…")
            } else {
                single
            };
            format!("bash: {trimmed}")
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
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::tool_title;
    use serde_json::json;

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
}
