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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use futures::FutureExt as _;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::collaboration_mode::{ModeKind, ModeSettings};
use crate::db::ThreadRecord;
use crate::goal::{self, GoalVerdict};
use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, LanguageModelToolResult, LanguageModelToolUse, MessageContent,
    ReasoningEffort, Role, StopReason, TokenUsage,
};
use crate::message::Message;
use crate::prefix_stability::StablePrefix;
use crate::proposed_plan::{ProposedPlanParser, ProposedPlanSegment};
use crate::title_state::TitleState;
use crate::token_meter::TokenMeter;
use crate::tool::{PermissionCache, PermissionDecision, ToolAuthorizationResponse, ToolRegistry};
use crate::tools;

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
    /// The call was not acted upon (overlay not shown or turn cancelled) —
    /// distinct from `Denied` (a real user rejection) so the UI does not
    /// render a "denied" verdict for a non-response. Used by the plan-approval
    /// flow's `Cancelled` arm.
    Cancelled,
}

/// User-facing approval policy for tool calls. Drives both the access chip in
/// the composer and the free / approval branch in `run_turn_loop`. Persisted on
/// the thread so switching threads restores the mode the user last picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Every approval-required tool call shows the authorization overlay.
    #[serde(rename = "on-request")]
    OnRequest,
    /// A built-in security-reviewer LLM agent vet each approval-required tool
    /// call. Safe calls run without prompting; risky ones still raise the
    /// overlay (with a one-line reason from the agent). Failures (timeout,
    /// parse error, unsupported verdict) fall back to the overlay.
    #[serde(rename = "auto-review")]
    #[default]
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
        /// The structured tool input, surfaced so the UI can derive aggregate
        /// counts by target (file path / command / pattern) without re-parsing
        /// the localized title. `None` only on historical rebuild paths that
        /// restore input from the persisted `MessageContent::ToolUse` instead.
        input: Option<serde_json::Value>,
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
    /// Emitted once when a sub-agent's child `Thread` is constructed and its
    /// first turn is about to start. Carries the parent-side `tool_use_id`,
    /// the sub-agent type + short description (from the model's `description`
    /// input field), and the live child `Entity<Thread>` so the UI can
    /// subscribe to it for the observation panel. UI-only: never enters the
    /// model's message history.
    SubagentStarted {
        id: String,
        subagent_type: String,
        description: String,
        child: gpui::Entity<Thread>,
    },
    /// A spawned sub-agent's aggregated progress, forwarded to the parent so
    /// observers can track lifecycle status while the sub-agent runs. Metrics
    /// remain available to the backend result envelope but are not rendered in
    /// the compact parent message row. UI-only: never enters the model's
    /// message history. `id` is the parent-side `agent` tool_use_id. Emitted by
    /// `SpawnAgentTool`'s subscription on the child thread's `ToolCall` /
    /// `ToolResult` / `TokenUsageUpdated` events.
    SubagentProgress {
        id: String,
        subagent_type: String,
        tool_uses: u32,
        token_usage: crate::language_model::TokenUsage,
        latest_activity: Option<String>,
        status: ToolCallStatus,
    },
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
    /// The completion loop has unwound and released the `running_turn` slot, so
    /// `is_running()` is now false. Unlike `Stop`, which is emitted for each
    /// provider round before recovery and steer-drain decisions, this is the
    /// safe boundary for starting queued follow-ups.
    TurnFinished {
        cancelled: bool,
        failed: bool,
        stranded_steer_ids: Vec<String>,
    },
    /// The provider is retrying the HTTP handshake after a transient failure
    /// (429 / 5xx / network error). The UI shows a retry badge; the next
    /// non-`Retry` event resolves it. `reason` / `detail` feed the badge and
    /// its expandable body.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
        detail: Option<String>,
    },
    /// An error during streaming.
    Error(anyhow::Error),
    /// Live `<proposed_plan>` block delta streamed from the model in Plan mode.
    /// UI-only: never enters the model's message history. The block content is
    /// stripped from the assistant message so the model never re-reads its own
    /// plan next turn.
    PlanDelta { delta: String },
    /// A turn ended with a complete `<proposed_plan>` block. The UI shows the
    /// Plan Implementation overlay; the user's choice arrives via the
    /// workspace's plan-review handler. UI-only: never enters history.
    PlanReady { plan_text: String },
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
    /// Reasoning effort changed. The UI persists the new value to the thread record.
    ReasoningEffortChanged { effort: ReasoningEffort },
    /// Goal mode toggled on/off. The UI shows or hides the `◎ /goal active`
    /// chip and, on activation, starts its elapsed-time ticker.
    GoalChanged { active: bool },
    /// Auto-compaction summarization pass started: the side-LLM call in
    /// `stream_summary` is in flight. The cockpit flips to a "Summarizing
    /// context…" phase until the matching `Compaction` event lands. Carries
    /// the active-token total that crossed the trigger threshold, mirroring
    /// `Compaction::tokens_before`.
    CompactionStarted { tokens_before: u64 },
    /// A compaction pass landed: older history was replaced by a handoff
    /// summary message. `messages_compacted` is the count of messages folded
    /// into the summary; `tokens_before` is the active-token total that
    /// triggered the pass. The UI renders a Recap card; the store records a
    /// `compaction` event in the timeline.
    Compaction {
        summary: String,
        messages_compacted: usize,
        tokens_before: u64,
    },
    /// A goal evaluator run completed. Carries the verdict + reason + the
    /// evaluation count so the UI can refresh the status popover without
    /// re-reading thread state. Emitted on both satisfied and unsatisfied
    /// outcomes; a satisfied outcome is immediately followed by
    /// `GoalChanged { active: false }` from `clear_goal`.
    GoalEvaluated {
        satisfied: bool,
        reason: String,
        evaluations: u32,
    },
    /// A peer message was delivered to this thread from another team member.
    /// Rendered as a `💬 from {name}` bubble, distinct from user/assistant
    /// turns. The thread also received a user-role message carrying the same
    /// content (so the model sees it); this event is the UI-side mirror.
    PeerMessage { from: String, content: String },
    /// A queued steer follow-up was drained into `messages` at the next safe
    /// join point — the running turn has now absorbed it and will see it on
    /// its next round. The UI moves the matching queued card into the
    /// conversation at this moment so the bubble's stream position marks the
    /// turn that was actually steered. Carries the drained message's id so the
    /// UI can correlate it back to the parked queue item.
    SteerInjected { message_id: String },
    /// A page-state notification from a built-in browser tab routed to the
    /// owning thread. `EvalResult` and `UserHandback` are resolved in the host
    /// (against pending oneshots) and never reach here; this event carries the
    /// page-state variants (`PageLoaded` / `DomChanged` / `Navigation`) so the
    /// conversation/UI can observe the tab the agent is driving. The host
    /// binds a tab to its owning thread at `open_tab` time, so a notification
    /// always lands on the thread that opened the tab, even after the active
    /// conversation switches.
    BrowserNotification {
        tab_id: crate::webview_host::BrowserTabId,
        notification: crate::webview_host::BrowserNotification,
    },
    /// An untrusted page requested an inbound write via
    /// `__manox_request_write__(intent, payload)`. This is orthogonal to the
    /// outbound `ApprovalMode` axis: every inbound request surfaces a
    /// confirmation overlay regardless of mode (a page must never gain a write
    /// path into manox just because the agent runs in Yolo). The UI resolves
    /// the prompt and calls `Thread::respond_inbound` with the decision; the
    /// host's parked oneshot — stored in `pending_inbound` — is the sink.
    InboundAuthorization {
        id: String,
        intent: String,
        payload: serde_json::Value,
    },
}

/// UI metadata for a pending tool-call authorization. Stored alongside the
/// oneshot sender so the workspace can re-emit `ToolCallAuthorization` when
/// the user switches back to a thread parked on an approval prompt.
pub struct PendingAuthMeta {
    pub tool_name: String,
    pub summary: String,
    pub input: serde_json::Value,
}

/// Session-scoped goal-mode state. Not persisted — a goal is an ephemeral
/// autonomy directive tied to the live session, so a reloaded thread always
/// starts with no goal (mirrors the `worktree` session-scoped pattern, and
/// `Instant` is not `Serialize` in any case).
pub struct GoalState {
    /// The completion condition the agent works toward, in the user's words.
    pub condition: String,
    /// Wall-clock start instant for the elapsed-time chip. Monotonic so the
    /// chip is unaffected by system-clock changes.
    pub started_at: Instant,
    /// Monotonic evaluation counter, incremented before each evaluator call.
    pub evaluations: u32,
    /// The last evaluator reason (satisfied or not), shown in the status
    /// popover so the user can see why the loop is or is not continuing.
    pub last_reason: String,
}

/// Outcome of a goal evaluation, deciding what `maybe_continue_goal` does next.
enum GoalAction {
    /// Condition not yet met — inject the condition and start another turn.
    Continue,
    /// Condition met — clear the goal and exit goal mode.
    Satisfied,
    /// Goal cleared mid-evaluation (cancel / `/goal clear`) or the evaluation
    /// cap was hit — drop the result and clear without continuing.
    Abort,
}

/// Per-turn cap on recovery continuations (tool-use JSON parse error +
/// max-tokens truncation). Guards the main thread — which has no `max_turns` —
/// against a model that loops on unparseable JSON or keeps hitting the output
/// budget. Each recovery continuation bumps `recovery_retries`; exceeding
/// this emits `ThreadEvent::Error` and ends the turn rather than spinning
/// forever.
const MAX_RECOVERY_ATTEMPTS: u32 = 2;

/// Whether the assistant's visible text announces a tool action the model then
/// failed to perform (no `tool_use` emitted). Catches the narrate-without-acting
/// loop seen in thread 480b2469, where the model writes "let me find the
/// relevant component code" / "让我先找到相关组件代码" each turn but emits no
/// `tool_use`, so the turn ends as a clean `EndTurn` and the user must manually
/// prod the model.
///
/// Conservative by design: only short visible text (under `MAX_LEN` chars, so a
/// genuine long answer is never mis-flagged) and a tight bilingual phrase set.
/// The degenerate-empty check cannot reach this case — it requires *no* visible
/// text. A false positive costs at most one nudge round-trip and is bounded by
/// `MAX_RECOVERY_ATTEMPTS`; a true positive breaks a loop that would otherwise
/// spin until the user intervenes.
fn announces_tool_intent_without_call(text: &str) -> bool {
    const MAX_LEN: usize = 400;
    if text.chars().count() >= MAX_LEN {
        return false;
    }
    // English intent phrases, matched case-insensitively as substrings (no
    // regex dependency — the agent crate stays regex-free). Curly apostrophes
    // are not normalized; models overwhelmingly emit straight ASCII quotes.
    const EN: &[&str] = &[
        "let me",
        "i'll",
        "i will",
        "let's",
        "going to",
        "i'm going",
        "i'm about",
    ];
    // Chinese intent phrases, matched verbatim (CJK has no case).
    const ZH: &[&str] = &["让我", "我现在", "我来", "接下来我", "我先", "我将"];
    let lower = text.to_ascii_lowercase();
    EN.iter().any(|p| lower.contains(p)) || ZH.iter().any(|p| text.contains(p))
}

/// Normalize an assistant message's content block list at finalization: drop
/// whitespace-only Text/Thinking blocks, defensively merge accidentally-adjacent
/// same-type blocks, and never merge Thinking blocks separated by Text (that
/// would erase the streaming timeline). The streaming append helpers already
/// coalesce consecutive same-type deltas, so adjacency should not arise during
/// a live stream — this is a backstop for crash-recovery restores and future
/// regressions. ToolUse blocks pass through untouched.
fn normalize_assistant_content(content: &mut Vec<MessageContent>) {
    let mut out: Vec<MessageContent> = Vec::with_capacity(content.len());
    for c in content.drain(..) {
        match c {
            MessageContent::Text(t) => {
                if t.trim().is_empty() {
                    continue;
                }
                match out.last_mut() {
                    Some(MessageContent::Text(existing)) => existing.push_str(&t),
                    _ => out.push(MessageContent::Text(t)),
                }
            }
            MessageContent::Thinking { text, signature } => {
                if text.trim().is_empty() {
                    continue;
                }
                match out.last_mut() {
                    Some(MessageContent::Thinking {
                        text: existing,
                        signature: sig,
                    }) => {
                        existing.push_str(&text);
                        if sig.is_none() {
                            *sig = signature;
                        }
                    }
                    _ => out.push(MessageContent::Thinking { text, signature }),
                }
            }
            other => out.push(other),
        }
    }
    *content = out;
}

/// What `run_turn_loop` does after a round-trip produced no executable tool
/// call. `Done` ends the turn (a clean `EndTurn` with nothing to retry);
/// `Continue` loops back to `build_completion_request` with the failure fed
/// back into history (a max-tokens directive or an error tool_result, both
/// append-only so the prefix cache stays intact); `Abort` ends the turn on the
/// recovery-retry cap. `CompactAndRetry` answers a context overflow: shrink
/// history with one compaction, then re-request — the only recovery that can
/// make an unsendable request sendable.
enum RecoveryAction {
    Done,
    Continue,
    Abort,
    CompactAndRetry,
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
    /// Active worktree state. `None` unless the thread has entered a git
    /// worktree via the `enter_worktree` tool. While `Some`, `cwd` is the
    /// worktree path and the tool registry carries a worktree-aware sandbox
    /// (bound repo `.git` writable, network on). Session-scoped — never
    /// persisted: a worktree is an ephemeral isolation context, not part of
    /// the thread record.
    worktree: Option<WorktreeState>,
    /// Active agents team, if this thread leads one. `None` for plain main
    /// threads without a team and for `agent`-spawned sub-agents. The leader
    /// thread owns the `Entity<Team>`; worker members are tracked inside it.
    /// Not persisted — a team is session-scoped coordination, not a recoverable
    /// conversation, so a reloaded thread always starts teamless.
    team: Option<Entity<crate::team::Team>>,
    /// tool_uses collected during the current turn, processed after the stream ends.
    pending_tool_uses: Vec<LanguageModelToolUse>,
    /// Steer follow-ups queued by the UI while a turn is running. Drained into
    /// `messages` at each safe join point — before every `build_completion_request`
    /// (so the running turn absorbs the steer in its next round) and at the
    /// EndTurn break (so a steer that arrived during the final assistant text is
    /// not orphaned). The drain happens after the previous round's tool results
    /// are already appended, keeping each `tool_use` paired with its
    /// `tool_result` ahead of any injected user message.
    pending_steer: std::collections::VecDeque<Message>,
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
    /// Whether the multi-level CLAUDE.md eager block is injected into this
    /// thread's requests. Always on except for the built-in `explore`
    /// sub-agent, which skips instruction files to stay fast and cheap
    /// (Claude Code's Explore/Plan carve-out).
    instructions_enabled: bool,
    /// Instruction files already injected by the lazy layer (nested CLAUDE.md
    /// and path-scoped rules discovered via `read_file`), so a directory's
    /// instructions ride exactly one tool result. Cleared when a compaction
    /// lands, since the summary may drop the reminders from the request —
    /// re-reading the directory then re-injects them.
    lazy_instructions_injected: HashSet<PathBuf>,
    /// Nesting depth. Main thread = 0; a sub-agent = parent depth + 1.
    depth: u32,
    /// Human-readable owner label: "lead" for the main thread, the
    /// subagent_type for an `agent`-spawned sub-agent, the member name for a
    /// team member. Surfaces as the write-lock owner so conflict errors name
    /// the agent holding a file.
    agent_label: String,
    /// Max agentic turns before a sub-agent is force-stopped. `None` = unlimited.
    max_turns: Option<u32>,
    /// Completed round-trips in the current turn, for `max_turns` enforcement.
    turn_count: u32,
    /// Whether the max-turns summary turn has already been injected. The cap is
    /// allowed one extra round-trip so the sub-agent can produce a coherent
    /// final message instead of ending mid-work; a second cap hit hard-stops.
    cap_summary_injected: bool,
    /// Set when a `ToolUseJsonParseError` landed this round: the placeholder
    /// tool_use and the error tool_result are already in history, so the loop
    /// continues to feed the failure back to the model instead of ending the
    /// turn dead on an orphaned error result (thread 76aef71a).
    pending_parse_error: bool,
    /// The error string of a stream that broke mid-turn — a
    /// `LanguageModelCompletionEvent::Err` arrived after the handshake. The
    /// empty-tool-use branch nudges the model to retry or wrap up instead of
    /// ending the turn dead on a half-finished assistant message (thread
    /// 0b80401d: the final summary round-trip was lost to a mid-stream failure
    /// with no recovery). Handshake-level failures (`stream_completion` itself
    /// returns `Err`) inject the nudge directly at the call site — no event
    /// was ever produced, so this flag is not needed for that path.
    pending_stream_error: Option<String>,
    /// Recovery continuations this turn (parse error or max-tokens truncation).
    /// Bumped on each retry and capped by `MAX_RECOVERY_ATTEMPTS`. Reset at
    /// `run_turn_loop` entry — it accumulates within a single turn's retries,
    /// not across turns.
    recovery_retries: u32,
    /// Set alongside `pending_stream_error` when the mid-stream error is a
    /// classified `ContextOverflow` — the empty-tool-use branch then routes to
    /// a single compact-and-retry instead of the generic nudge, because
    /// re-sending the identical oversized request can never succeed.
    pending_context_overflow: bool,
    /// Whether this turn already spent its one compact-and-retry after a
    /// context overflow. Set when the recovery fires; cleared when a request
    /// completes without a stream error (and at turn entry), so a later,
    /// genuinely-new overflow earns its own recovery while a repeated one
    /// aborts the turn instead of looping compactions.
    overflow_recovery_attempted: bool,
    /// The `StopReason` of the most recent completion, inspected by the
    /// empty-tool-use branch to detect max-tokens truncation. Reset at the top
    /// of each round-trip and after a recovery turn so a prior stop reason
    /// never bleeds forward.
    last_stop_reason: Option<StopReason>,
    /// The running turn task; dropping it aborts the turn.
    running_turn: Option<Task<()>>,
    /// Cancellation token for the running turn. Cancelled by `cancel()` so the
    /// in-flight tool (e.g. `bash`) can reap its process group promptly instead
    /// of relying on task-drop, which does not reach the detached tokio child.
    turn_cancel: Option<CancellationToken>,
    /// Wall-clock anchor for the in-flight turn, set the instant `TurnStarted`
    /// is emitted and cleared when the turn ends (`Stop`/`Error`/cancel). The
    /// cockpit reads `elapsed()` for its run-status timer; `None` ⇒ idle.
    /// Mirrors `GoalState::started_at` — a core-side anchor that travels with
    /// the entity across thread swaps, so the UI does not re-wire on switch.
    turn_started_at: Option<Instant>,
    /// Optional tool whitelist for the current turn, set by a slash command's
    /// `allowed-tools` frontmatter. `None` or empty = inherit all tools. The
    /// filter lasts for the turn's lifetime and is cleared when the turn ends,
    /// so it never leaks into a subsequent free-form message.
    turn_tool_filter: Option<Vec<String>>,
    /// Whether the SessionStart hook has fired for this thread. Main threads
    /// (depth 0) fire once on their first turn; sub-agents never fire it.
    session_started: bool,
    /// The active collaboration mode. `Plan` filters the tool set to
    /// read-only tools and the model submits a plan by emitting a single
    /// `<proposed_plan>` block (parsed by `proposed_plan_parser`); `Default`
    /// is the execution mode. Session-scoped — never persisted: a reloaded
    /// thread always starts in `Default`. Main-thread only in spirit; the
    /// per-mode `developer_instructions` are injected as a fixed-position
    /// `<collaboration_mode>` message at request-build time, not woven into
    /// the system prompt, so toggling mode keeps the provider prefix cache
    /// warm. Sub-agents always run in `Default`.
    collaboration_mode: ModeKind,
    /// Streaming `<proposed_plan>` parser for the current turn. Reset at the
    /// top of each turn iteration. Splits assistant text deltas into visible
    /// text (persisted into the assistant message) and plan deltas (emitted as
    /// `ThreadEvent::PlanDelta`); a completed block at turn end yields
    /// `ThreadEvent::PlanReady`.
    proposed_plan_parser: ProposedPlanParser,
    /// Accumulated text of the current turn's `<proposed_plan>` block (the
    /// concatenated `Delta` segments the parser emits). Drains into
    /// `ThreadEvent::PlanReady` at turn end, then clears. Empty when the turn
    /// produced no plan.
    pending_plan_text: String,
    /// Whether the current turn emitted any `<proposed_plan>` block content.
    /// Set on `PlanDelta` and reset at turn top, it exempts the turn from the
    /// degenerate-empty-turn check: a turn that produced a plan block has
    /// produced its intended output even though the block content never enters
    /// the assistant message (and thus the visible-text check sees nothing).
    produced_plan_this_turn: bool,
    /// User-selected reasoning effort for providers that expose an effort knob.
    /// This is request metadata, not model-visible prompt text.
    reasoning_effort: ReasoningEffort,
    /// Pending inbound-write authorization oneshots for the browser axis,
    /// keyed by the inbound request id allocated by the host. Resolved by
    /// `respond_inbound` with an allow/deny decision; orthogonal to
    /// `approval_mode` (a page's write request is never auto-allowed).
    pending_inbound: HashMap<String, tokio::sync::oneshot::Sender<bool>>,
    /// Active goal state. `None` unless the user set a completion condition via
    /// `/goal <condition>`. While `Some`, after each natural turn end the
    /// thread runs a lightweight evaluator; an unsatisfied goal auto-continues
    /// the turn (condition as directive), a satisfied goal clears itself and
    /// exits goal mode. Session-scoped — never persisted. Main-thread only —
    /// sub-agents never carry a goal (`set_goal` no-ops at depth > 0).
    goal: Option<GoalState>,
    /// Cancellation token for an in-flight goal evaluator. `None` when no
    /// evaluation is running. Cancelled by `cancel()`, `clear_goal`, and on
    /// satisfaction so a stray late evaluator result does not trigger a
    /// continuation. Mirrors `turn_cancel`'s role for the main turn.
    goal_cancel: Option<CancellationToken>,
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
    /// In-process HTTP proxy for the restricted-network sandbox policy.
    /// Spawned lazily on the first sandboxed bash call when the thread's
    /// sandbox `NetworkPolicy` is `Restricted`; dropped when the policy
    /// changes (worktree enter/exit) so the next call re-spawns against
    /// the new allowlist. `None` when the policy is `Blocked` or
    /// `Unrestricted` (no proxy needed).
    proxy: Option<http_proxy::ProxyHandle>,
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
    /// Persisted UI annotations (`Error` / `Notice` cards) loaded from the
    /// `thread_ui_notes` table. A UI-only cache: it is never part of the
    /// canonical `messages`, never read by `build_completion_request`, and
    /// never written by `snapshot` — so it cannot perturb the request
    /// prefix or prompt-cache hits. Reload splices these back into the
    /// conversation at the end of their owning turn.
    ui_notes: Vec<crate::db::UiNoteRecord>,
}

/// A forwarded authorization request from a sub-agent: which child thread holds
/// the pending decision and what id the child knows it by.
struct ChildAuthRoute {
    child: WeakEntity<Thread>,
    child_auth_id: String,
}

/// Session-scoped state for a thread currently inside a git worktree. Not
/// persisted — a worktree is an ephemeral isolation context tied to the live
/// session, so a reloaded thread always starts outside any worktree.
pub struct WorktreeState {
    /// Absolute path of the worktree directory (equals `Thread::cwd` while
    /// active).
    pub path: PathBuf,
    /// cwd to restore on exit — the project root or launch dir the thread was
    /// in before entering the worktree.
    pub prior_cwd: PathBuf,
    /// The branch the worktree was created on. Used for cleanup (`git branch
    /// -D`) and the UI status chip.
    pub branch: String,
    /// The bound repo's shared `.git` directory (`git rev-parse
    /// --git-common-dir` from inside the worktree). The worktree-aware sandbox
    /// de-protects this path so git ops against the main repo succeed.
    pub git_common_dir: PathBuf,
    /// Whether the worktree was created by a sub-agent (P3 isolation). Such
    /// worktrees are auto-removed on session end when clean; user-entered
    /// worktrees are left for the user to return to.
    pub subagent_created: bool,
}

impl EventEmitter<ThreadEvent> for Thread {}

impl Drop for Thread {
    /// Auto-remove a sub-agent's worktree when it is clean, so isolated
    /// sub-agent runs do not litter `.claude/worktrees/`. A dirty worktree is
    /// left on disk for the user to inspect. Fire-and-forget on the tokio
    /// runtime — `Drop` cannot await, and the runtime may already be gone at
    /// process teardown, so a missing handle is a quiet no-op. User-entered
    /// worktrees (`subagent_created == false`) are never auto-removed here;
    /// they live until `exit_worktree` with `action=remove`.
    fn drop(&mut self) {
        let Some(wt) = self.worktree.take() else {
            return;
        };
        if !wt.subagent_created {
            return;
        }
        let Some(handle) = crate::runtime::try_handle() else {
            return;
        };
        let path = wt.path.clone();
        let branch = wt.branch.clone();
        // Run `git worktree remove` from the main repo root (the bound
        // `.git`'s parent) so the worktree being removed is not its own cwd.
        let repo_root = wt
            .git_common_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(path.clone());
        handle.spawn(async move {
            let status = tokio::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&path)
                .output()
                .await;
            let clean = matches!(status, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim().is_empty());
            if !clean {
                tracing::debug!(
                    worktree = %path.display(),
                    "subagent worktree dirty or status failed — left on disk for inspection"
                );
                return;
            }
            let path_str = path.display().to_string();
            // Remove the worktree first; only delete the branch on success so a
            // failed remove (e.g. a concurrent write between the clean check
            // and the remove) does not leave a branch-less orphan worktree.
            let remove = tokio::process::Command::new("git")
                .args(["worktree", "remove", &path_str])
                .current_dir(&repo_root)
                .output()
                .await;
            let removed = matches!(&remove, Ok(o) if o.status.success());
            if !removed {
                let stderr = remove
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                    .unwrap_or_default();
                tracing::warn!(
                    worktree = %path.display(),
                    stderr = %stderr,
                    "git worktree remove failed — worktree left on disk, branch preserved"
                );
                return;
            }
            let branch_del = tokio::process::Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(&repo_root)
                .output()
                .await;
            if !matches!(branch_del, Ok(o) if o.status.success()) {
                tracing::warn!(
                    branch = %branch,
                    "worktree removed but branch -D failed — branch left behind"
                );
            }
        });
    }
}

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
                worktree: None,
                team: None,
                pending_tool_uses: Vec::new(),
                pending_steer: std::collections::VecDeque::new(),
                pending_authorizations: HashMap::new(),
                pending_auth_meta: HashMap::new(),
                pending_inbound: HashMap::new(),
                pending_child_auth: HashMap::new(),
                system: None,
                instructions_enabled: true,
                lazy_instructions_injected: HashSet::new(),
                depth: 0,
                agent_label: "lead".to_string(),
                max_turns: None,
                turn_count: 0,
                cap_summary_injected: false,
                pending_parse_error: false,
                pending_stream_error: None,
                recovery_retries: 0,
                pending_context_overflow: false,
                overflow_recovery_attempted: false,
                last_stop_reason: None,
                running_turn: None,
                turn_started_at: None,
                turn_cancel: None,
                turn_tool_filter: None,
                session_started: false,
                collaboration_mode: ModeKind::Default,
                proposed_plan_parser: ProposedPlanParser::new(),
                pending_plan_text: String::new(),
                produced_plan_this_turn: false,
                reasoning_effort: ReasoningEffort::default(),
                goal: None,
                goal_cancel: None,
                title_state: TitleState::default(),
                prefix_stability: StablePrefix::default(),
                proxy: None,
                provider_id,
                parent_id: None,
                archived: false,
                pinned: false,
                created_at: now,
                interacted_at: now,
                token_meter: TokenMeter::default(),
                persist_revision: AtomicU64::new(0),
                ui_notes: Vec::new(),
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
                worktree: None,
                team: None,
                pending_tool_uses: Vec::new(),
                pending_steer: std::collections::VecDeque::new(),
                pending_authorizations: HashMap::new(),
                pending_auth_meta: HashMap::new(),
                pending_inbound: HashMap::new(),
                pending_child_auth: HashMap::new(),
                system: None,
                instructions_enabled: true,
                lazy_instructions_injected: HashSet::new(),
                depth: rec.depth as u32,
                agent_label: "lead".to_string(),
                max_turns: None,
                turn_count: 0,
                cap_summary_injected: false,
                pending_parse_error: false,
                pending_stream_error: None,
                recovery_retries: 0,
                pending_context_overflow: false,
                overflow_recovery_attempted: false,
                last_stop_reason: None,
                running_turn: None,
                turn_started_at: None,
                turn_cancel: None,
                turn_tool_filter: None,
                session_started: false,
                collaboration_mode: ModeKind::Default,
                proposed_plan_parser: ProposedPlanParser::new(),
                pending_plan_text: String::new(),
                produced_plan_this_turn: false,
                reasoning_effort: ReasoningEffort::from_i64(rec.reasoning_effort),
                goal: None,
                goal_cancel: None,
                title_state: TitleState::restore(
                    rec.title,
                    rec.title_override,
                    title_last_eval_user_count,
                ),
                prefix_stability: StablePrefix::default(),
                proxy: None,
                provider_id,
                parent_id: rec.parent_id,
                archived: rec.archived,
                pinned: rec.pinned,
                created_at: rec.created_at,
                interacted_at: rec.interacted_at,
                token_meter: TokenMeter::restore(
                    rec.cumulative_token_usage,
                    rec.request_token_usage,
                    rec.per_model_token_usage,
                ),
                persist_revision: AtomicU64::new(rec.revision),
                ui_notes: Vec::new(),
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
        reasoning_effort: ReasoningEffort,
        system: String,
        max_turns: u32,
        depth: u32,
        agent_label: String,
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
                worktree: None,
                team: None,
                pending_tool_uses: Vec::new(),
                pending_steer: std::collections::VecDeque::new(),
                pending_authorizations: HashMap::new(),
                pending_auth_meta: HashMap::new(),
                pending_inbound: HashMap::new(),
                pending_child_auth: HashMap::new(),
                system: Some(system),
                instructions_enabled: true,
                lazy_instructions_injected: HashSet::new(),
                depth,
                agent_label,
                max_turns: Some(max_turns),
                turn_count: 0,
                cap_summary_injected: false,
                pending_parse_error: false,
                pending_stream_error: None,
                recovery_retries: 0,
                pending_context_overflow: false,
                overflow_recovery_attempted: false,
                last_stop_reason: None,
                running_turn: None,
                turn_started_at: None,
                turn_cancel: None,
                turn_tool_filter: None,
                session_started: false,
                collaboration_mode: ModeKind::Default,
                proposed_plan_parser: ProposedPlanParser::new(),
                pending_plan_text: String::new(),
                produced_plan_this_turn: false,
                reasoning_effort,
                goal: None,
                goal_cancel: None,
                title_state: TitleState::default(),
                prefix_stability: StablePrefix::default(),
                proxy: None,
                provider_id: None,
                parent_id: None,
                archived: false,
                pinned: false,
                created_at: chrono::Utc::now().timestamp(),
                interacted_at: chrono::Utc::now().timestamp(),
                token_meter: TokenMeter::default(),
                persist_revision: AtomicU64::new(0),
                ui_notes: Vec::new(),
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
            reasoning_effort: self.reasoning_effort.as_i64(),
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
            per_model_token_usage: self.token_meter.per_model().clone(),
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
                    MessageContent::Compaction(summary) => {
                        out.push_str("> *compacted summary:* ");
                        out.push_str(summary);
                        out.push('\n');
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

    /// Register a pending inbound-write request and surface its confirmation
    /// overlay. The host allocates the request id, creates the oneshot, parks
    /// the `Sender` here, and emits `ThreadEvent::InboundAuthorization` so the
    /// UI can prompt; the parked `Receiver` is awaited in the host. Resolution
    /// arrives via [`respond_inbound`](Self::respond_inbound).
    pub fn register_inbound(
        &mut self,
        id: String,
        intent: String,
        payload: serde_json::Value,
        responder: tokio::sync::oneshot::Sender<bool>,
        cx: &mut Context<Self>,
    ) {
        // The confirmation overlay that resolves these is wired in a later
        // step; until then a flooding page could grow this map unbounded
        // until the thread is cancelled / released. Cap it: a request past
        // the cap is dropped (the responder is dropped here → the host's
        // parked await unparks with a cancellation).
        const MAX_PENDING_INBOUND: usize = 64;
        if self.pending_inbound.len() >= MAX_PENDING_INBOUND {
            tracing::warn!(
                pending = self.pending_inbound.len(),
                id = %id,
                "browser inbound queue saturated; dropping inbound-write request"
            );
            return;
        }
        self.pending_inbound.insert(id.clone(), responder);
        cx.emit(ThreadEvent::InboundAuthorization {
            id,
            intent,
            payload,
        });
    }

    /// Resolve a pending inbound-write request by id with an allow/deny
    /// decision. The host's parked `Receiver` unparks; a stale or unknown id
    /// silently no-ops (the request was cancelled, resolved, or the tab was
    /// closed and the sender dropped).
    pub fn respond_inbound(&mut self, id: &str, allowed: bool, _cx: &mut Context<Self>) {
        if let Some(tx) = self.pending_inbound.remove(id) {
            let _ = tx.send(allowed);
        }
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

    // `SubagentProgress` is emitted directly by `SpawnAgentTool`'s
    // `forward_progress` helper via `cx.emit` on the parent entity — see
    // `tools/agent.rs`. Kept off `Thread` so the event stays a pure emit
    // without a many-argument forwarding method.

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn agent_label(&self) -> &str {
        &self.agent_label
    }

    /// The active team this thread leads, if any. `None` for non-team threads.
    pub fn team(&self) -> Option<&Entity<crate::team::Team>> {
        self.team.as_ref()
    }

    /// Attach a team this thread leads. Called by `team_create`. Does not
    /// touch the tool registry — team tools are advertised from the start and
    /// no-op until a team exists, so the request-tool prefix is unaffected.
    pub fn set_team(&mut self, team: Entity<crate::team::Team>, cx: &mut Context<Self>) {
        self.team = Some(team);
        cx.notify();
    }

    /// Detach the team. `team_disband` calls this on the leader and on every
    /// member to break the team↔member strong cycle before the roster drops.
    pub fn clear_team(&mut self, cx: &mut Context<Self>) {
        self.team = None;
        cx.notify();
    }

    /// Deliver peer messages: append each as a user-role message, emit a
    /// `PeerMessage` event per message (UI mirror), then start one turn so the
    /// model sees them together. An empty slice is a no-op so unrelated
    /// callers don't synthesize a turn. The `[from {name}]:` prefix is
    /// append-only, so per-thread prefix-cache stability holds.
    pub fn deliver_peer_messages(
        &mut self,
        msgs: Vec<crate::team::PeerMessage>,
        cx: &mut Context<Self>,
    ) {
        if msgs.is_empty() {
            return;
        }
        for msg in &msgs {
            self.insert_user_message(
                crate::prompt::render(
                    crate::prompt::PromptTemplate::WrapperPeerMessage,
                    &crate::prompt::PeerMessageData {
                        from: msg.from.clone(),
                        content: msg.content.clone(),
                    },
                )
                .expect("peer message template render"),
                cx,
            );
            cx.emit(ThreadEvent::PeerMessage {
                from: msg.from.clone(),
                content: msg.content.clone(),
            });
        }
        self.run_turn(cx);
    }

    /// Test-only: occupy or clear the `running_turn` slot to simulate a busy
    /// vs idle thread in routing tests (the team router branches on
    /// `is_running`, which reads this field).
    #[cfg(test)]
    pub(crate) fn set_running_turn_for_test(&mut self, t: Option<Task<()>>) {
        self.running_turn = t;
    }

    /// The thread's active collaboration mode.
    pub fn collaboration_mode(&self) -> ModeKind {
        self.collaboration_mode
    }

    /// Switch collaboration mode. Called by the UI (slash `/plan`, the `+` menu
    /// row, or shift-tab cycling). Only sets the mode; the next
    /// `build_completion_request` resolves the per-mode tool set, model,
    /// reasoning effort, and `developer_instructions` injection. Does not start
    /// a turn. Goal mode and Plan mode are mutually exclusive — entering Plan
    /// clears an active goal so the next turn advertises write tools rather
    /// than the read-only plan set.
    pub fn set_collaboration_mode(&mut self, mode: ModeKind, cx: &mut Context<Self>) {
        if mode == ModeKind::Plan && self.goal.is_some() {
            self.clear_goal(cx);
        }
        self.collaboration_mode = mode;
        cx.notify();
    }

    /// Seed the approved plan as a user message (exiting Plan mode) without
    /// starting a turn. The `<proposed_plan>` block was never persisted into
    /// the assistant message, so the model needs the plan text re-injected to
    /// execute it. `ui` is the same UI metadata the live view stamps onto the
    /// verdict bubble, so a reloaded thread rebuilds the identical bubble
    /// (model/approval badges). Split from [`implement_approved_plan`] so a
    /// clear-context verdict can attach the thread before running.
    pub fn seed_approved_plan(
        &mut self,
        plan_text: String,
        ui: Option<crate::MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        self.set_collaboration_mode(ModeKind::Default, cx);
        let text = crate::collaboration_mode::implement_plan_user_message(&plan_text);
        self.insert_user_message_with_ui_metadata(text, ui, cx);
    }

    /// Seed the approved plan and launch a Default-mode turn for it. Used by
    /// the non-clear `Implement` verdict, where the thread is already the
    /// foreground view and turn events stream straight into the live
    /// conversation. The clear-context verdict does not call this — it swaps
    /// to a fresh thread at the workspace layer (archive old + spawn new),
    /// seeding the new thread with [`seed_approved_plan`] before attaching and
    /// running.
    pub fn implement_approved_plan(
        &mut self,
        plan_text: String,
        ui: Option<crate::MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        self.seed_approved_plan(plan_text, ui, cx);
        self.run_turn(cx);
    }

    /// Resolve the active mode's effective settings: built-in preset overlaid
    /// with the user's `[modes.*]` override from `settings.toml`. Request-build
    /// reads this for the tool set, `reasoning_effort`, `model`, and the
    /// `developer_instructions` injection.
    fn active_mode_settings(&self) -> ModeSettings {
        crate::collaboration_mode::resolve(self.collaboration_mode, &crate::settings::modes())
    }

    /// The model that serves this request: the mode's `model` override
    /// (resolved through the alias registry) when set, else the thread's
    /// selected model. Resolved fresh each call — the registry is a HashMap
    /// and the result is mode-stable across a session, so the prefix cache is
    /// unaffected.
    fn active_model(&self) -> Option<crate::language_model::AnyLanguageModel> {
        let mode = self.active_mode_settings();
        mode.model
            .as_deref()
            .and_then(crate::model_alias::resolve_model_ref)
            .or_else(|| self.model.clone())
    }

    /// The reasoning effort that serves this request: the mode's override
    /// when set, else the thread's user-selected effort.
    fn active_reasoning_effort(&self) -> ReasoningEffort {
        let mode = self.active_mode_settings();
        mode.reasoning_effort.unwrap_or(self.reasoning_effort)
    }

    /// Active goal state, if any. Drives the composer's `◎ /goal active` chip.
    pub fn goal(&self) -> Option<&GoalState> {
        self.goal.as_ref()
    }

    /// Set a completion condition and enter goal mode. Main-thread only — a
    /// sub-agent (depth > 0) no-ops. Exits Plan mode (the two are mutually
    /// exclusive) so the next turn advertises write tools. Does NOT start a
    /// turn — the caller owns turn initiation, exactly like
    /// `set_collaboration_mode`.
    pub fn set_goal(&mut self, condition: String, cx: &mut Context<Self>) {
        if self.depth != 0 {
            return;
        }
        if self.collaboration_mode == ModeKind::Plan {
            self.collaboration_mode = ModeKind::Default;
        }
        self.goal = Some(GoalState {
            condition,
            started_at: Instant::now(),
            evaluations: 0,
            last_reason: String::new(),
        });
        cx.emit(ThreadEvent::GoalChanged { active: true });
        cx.notify();
    }

    /// Clear the active goal and abort any in-flight evaluator. Called by
    /// `/goal clear`, on satisfaction, from `set_collaboration_mode` (mutual
    /// exclusion), and as a backstop from `cancel()`. Emits `GoalChanged {
    /// active: false }` when a goal was actually active so the chip disappears.
    pub fn clear_goal(&mut self, cx: &mut Context<Self>) {
        let was_active = self.goal.is_some();
        if let Some(c) = self.goal_cancel.take() {
            c.cancel();
        }
        self.goal = None;
        if was_active {
            cx.emit(ThreadEvent::GoalChanged { active: false });
            cx.notify();
        }
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.reasoning_effort
    }

    pub fn set_reasoning_effort(&mut self, effort: ReasoningEffort, cx: &mut Context<Self>) {
        if self.reasoning_effort == effort {
            return;
        }
        self.reasoning_effort = effort;
        cx.emit(ThreadEvent::ReasoningEffortChanged { effort });
        cx.notify();
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

    /// Whether any tool is in the session always-allow set. The cockpit
    /// permission indicator surfaces this as a "session allowlist active" tag.
    pub fn session_allowlist_active(&self) -> bool {
        self.permission.allowed_count() > 0
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

    /// Persisted UI annotations loaded from `thread_ui_notes`. UI-only cache:
    /// not part of `messages()`, not read by request building — see the field
    /// doc on `Thread::ui_notes`.
    pub fn ui_notes(&self) -> &[crate::db::UiNoteRecord] {
        &self.ui_notes
    }

    /// Replace the loaded UI-note cache. Called by `thread_store::load_thread`
    /// after `restore`; sub-agent and fresh-thread paths leave it empty.
    pub fn set_ui_notes(&mut self, notes: Vec<crate::db::UiNoteRecord>) {
        self.ui_notes = notes;
    }

    /// Append a UI note to the in-memory cache so a background thread that is
    /// reclaimed without a db reload still reproduces it on rebuild. The
    /// placeholder row (`id`/`seq`/`ts` unset) is discarded wholesale the next
    /// time `set_ui_notes` loads from db.
    pub fn push_ui_note(&mut self, note: crate::db::UiNoteRecord) {
        self.ui_notes.push(note);
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
    /// Also the anchor for persisting a UI note to the turn it belongs to.
    pub fn last_user_message_id(&self) -> Option<&str> {
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

    /// Toggle the multi-level CLAUDE.md eager block for this thread. Disabled
    /// only for the built-in `explore` sub-agent (Claude Code's Explore/Plan
    /// carve-out); every other thread keeps instructions on.
    pub fn set_instructions_enabled(&mut self, enabled: bool) {
        self.instructions_enabled = enabled;
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

    /// Active worktree state, if the thread has entered a git worktree.
    pub fn worktree(&self) -> Option<&WorktreeState> {
        self.worktree.as_ref()
    }

    /// Enter a git worktree: switch the session cwd to `path`, rebuild the
    /// tool registry with a worktree-aware sandbox (bound repo `.git`
    /// writable, network on), and stash the prior cwd for `exit_worktree` to
    /// restore. Unlike `set_project`, this is an explicit mid-conversation
    /// harness operation — the model directs the transition via the
    /// `enter_worktree` tool, so the messages-nonempty gate does not apply.
    ///
    /// `git_common_dir` is `git rev-parse --git-common-dir` resolved from
    /// inside the worktree (the main repo's shared `.git`). The caller (the
    /// worktree tool) has already run `git worktree add` before invoking this.
    pub fn enter_worktree(
        &mut self,
        path: PathBuf,
        branch: String,
        git_common_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let prior_cwd = self.cwd.clone();
        let project_root = self.project.clone().unwrap_or_else(|| prior_cwd.clone());
        let sandbox = self
            .sandbox_anchor(&project_root)
            .with_worktree(&path, &git_common_dir);
        // The network policy changes (worktree = Unrestricted); drop the
        // restricted-network proxy so the next bash call doesn't use a stale
        // port. The next call re-spawns if the new policy needs it.
        self.drop_network_proxy();
        self.cwd = path.clone();
        self.tools = Arc::new(tools::main_registry_with_policy(
            path.clone(),
            sandbox,
            cx.weak_entity(),
        ));
        self.worktree = Some(WorktreeState {
            path,
            prior_cwd,
            branch,
            git_common_dir,
            subagent_created: false,
        });
        // A worktree transition changes the system-prompt cwd line and the
        // tool registry's baked sandbox — force a clean prefix re-baseline so
        // the next turn captures the new prefix as a version bump rather than
        // silent drift.
        self.prefix_stability.invalidate();
        cx.notify();
    }

    /// Leave the active worktree. `Keep` switches cwd back but leaves the
    /// worktree and branch on disk; `Remove` also deletes them, refusing when
    /// the working tree is dirty unless `discard_changes` is set. The caller
    /// (the `exit_worktree` tool) is responsible for the actual `git worktree
    /// remove` shell-out when dirty-checking is needed; this method only
    /// restores thread state once the git side has succeeded.
    pub fn exit_worktree(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(wt) = self.worktree.take() else {
            return Err("Not in a worktree.".to_string());
        };
        let prior_cwd = wt.prior_cwd.clone();
        // Exiting a worktree restores the project-level network policy
        // (possibly Restricted); drop the worktree's Unrestricted proxy (if
        // any) so the next bash call re-spawns under the new policy.
        self.drop_network_proxy();
        self.cwd = prior_cwd.clone();
        let sandbox = self.sandbox_anchor(self.project.as_deref().unwrap_or(&prior_cwd));
        self.tools = Arc::new(tools::main_registry_with_policy(
            prior_cwd,
            sandbox,
            cx.weak_entity(),
        ));
        self.prefix_stability.invalidate();
        cx.notify();
        Ok(())
    }

    /// The base sandbox anchor (project root or launch cwd) without any
    /// worktree relaxation. Factored so `enter_worktree` extends it with
    /// `.with_worktree` and `exit_worktree` restores the plain policy.
    fn sandbox_anchor(&self, root: &Path) -> crate::sandbox::SandboxPolicy {
        crate::sandbox::SandboxPolicy::for_project(root)
    }

    /// The loopback port of the thread's in-process network proxy, if the
    /// sandbox network policy is `Restricted` and the proxy has been spawned.
    /// `None` when the policy is `Blocked` or `Unrestricted`, or when the
    /// proxy hasn't been spawned yet (it's spawned lazily on the first
    /// sandboxed bash call).
    pub fn network_proxy_port(&self) -> Option<u16> {
        self.proxy.as_ref().map(|p| p.port())
    }

    /// Ensure the thread's in-process HTTP proxy is running and return its
    /// port. Spawns the proxy lazily when the sandbox network policy is
    /// `Restricted`; returns `None` for `Blocked` or `Unrestricted` (no
    /// proxy needed). If the proxy is already running, returns its port
    /// without re-spawning.
    pub fn ensure_network_proxy(&mut self) -> Option<u16> {
        // Fast path: proxy already running, return its port without
        // rebuilding the sandbox policy (which reads settings.toml and
        // canonicalizes paths).
        if let Some(port) = self.proxy.as_ref().map(|p| p.port()) {
            return Some(port);
        }
        let sandbox = self.sandbox_for_bash();
        match sandbox.network() {
            crate::sandbox::NetworkPolicy::Restricted { allowlist } => {
                if self.proxy.is_none() {
                    let (events_tx, events_rx) = futures::channel::mpsc::unbounded();
                    let config = http_proxy::ProxyConfig {
                        allowlist: allowlist.clone(),
                        events: events_tx,
                    };
                    match http_proxy::ProxyHandle::spawn(config) {
                        Ok(handle) => {
                            self.proxy = Some(handle);
                            // Drain proxy events on a background thread so
                            // the unbounded channel doesn't fill and block
                            // connection threads (which send synchronously).
                            // Events are logged at debug level for diagnostics.
                            crate::runtime::handle().spawn(async move {
                                use futures::StreamExt as _;
                                let mut rx = events_rx;
                                while let Some(event) = rx.next().await {
                                    match event {
                                        http_proxy::ProxyEvent::Ready { port } => {
                                            tracing::debug!(port, "sandbox network proxy ready");
                                        }
                                        http_proxy::ProxyEvent::RequestAttempt {
                                            host,
                                            port,
                                            method,
                                            outcome,
                                        } => {
                                            tracing::debug!(
                                                host,
                                                port,
                                                method = method.as_str(),
                                                allowed = matches!(
                                                    outcome,
                                                    http_proxy::RequestOutcome::Allowed
                                                ),
                                                "proxy request"
                                            );
                                        }
                                        http_proxy::ProxyEvent::RequestCompleted {
                                            host,
                                            port,
                                            method,
                                            bytes_to_remote,
                                            bytes_from_remote,
                                            duration_ms,
                                        } => {
                                            tracing::debug!(
                                                host,
                                                port,
                                                method = method.as_str(),
                                                bytes_to_remote,
                                                bytes_from_remote,
                                                duration_ms,
                                                "proxy request completed"
                                            );
                                        }
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "failed to spawn sandbox network proxy; \
                                 sandboxed bash will have no network access"
                            );
                            return None;
                        }
                    }
                }
                self.proxy.as_ref().map(|p| p.port())
            }
            crate::sandbox::NetworkPolicy::Blocked => None,
            crate::sandbox::NetworkPolicy::Unrestricted => None,
        }
    }

    /// Drop the active network proxy, if any. Called when the sandbox policy
    /// changes (worktree enter/exit) so the next bash call re-spawns against
    /// the new policy. The proxy `Drop` joins a listener thread; to avoid
    /// blocking the gpui main thread, the old handle is sent to a tokio task
    /// for teardown.
    fn drop_network_proxy(&mut self) {
        if let Some(proxy) = self.proxy.take() {
            crate::runtime::handle().spawn(async move {
                drop(proxy);
            });
        }
    }

    /// The sandbox policy the bash tool should use. Derived from the active
    /// worktree state (if any) or the project root. This mirrors what
    /// `enter_worktree`/`exit_worktree` bake into the tool registry, so the
    /// proxy lifecycle stays in sync with the registry's sandbox.
    fn sandbox_for_bash(&self) -> crate::sandbox::SandboxPolicy {
        if let Some(wt) = &self.worktree {
            let project_root = self.project.clone().unwrap_or_else(|| self.cwd.clone());
            crate::sandbox::SandboxPolicy::for_project(&project_root)
                .with_worktree(&wt.path, &wt.git_common_dir)
        } else {
            let root = self.project.clone().unwrap_or_else(|| self.cwd.clone());
            crate::sandbox::SandboxPolicy::for_project(&root)
        }
    }

    /// Stamp an initial worktree state on a freshly-constructed sub-agent whose
    /// tool registry was already built worktree-aware (P3 `isolation: worktree`
    /// path). Unlike `enter_worktree`, this does NOT rebuild the registry — the
    /// child's registry carries the `for_worktree` sandbox from construction.
    /// Sets the state field so the system prompt advertises the worktree and
    /// session-end auto-cleanup (Drop) can remove it when clean.
    pub(crate) fn set_worktree_state(&mut self, state: WorktreeState, cx: &mut Context<Self>) {
        self.worktree = Some(state);
        self.prefix_stability.invalidate();
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        self.running_turn.is_some()
    }

    /// Wall-clock anchor of the in-flight turn, or `None` when idle. The
    /// cockpit reads `elapsed()` for its run-status timer.
    pub fn turn_started_at(&self) -> Option<Instant> {
        self.turn_started_at
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
        self.insert_user_message_with_ui_metadata(text, None, cx);
    }

    pub fn insert_user_message_with_ui_metadata(
        &mut self,
        text: String,
        ui: Option<crate::MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        let mut message = Message::user(text);
        message.ui = ui;
        self.messages.push(message);
        cx.notify();
    }

    /// Append a user message carrying multiple content blocks (e.g. text plus attached images).
    pub fn insert_user_message_with_content(
        &mut self,
        content: Vec<MessageContent>,
        cx: &mut Context<Self>,
    ) {
        self.insert_user_message_with_content_and_ui_metadata(content, None, cx);
    }

    pub fn insert_user_message_with_content_and_ui_metadata(
        &mut self,
        content: Vec<MessageContent>,
        ui: Option<crate::MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) {
        let mut message = Message::user_with_content(content);
        message.ui = ui;
        self.messages.push(message);
        cx.notify();
    }

    /// Queue a steer follow-up — a user message that the running turn absorbs
    /// at the next safe join point rather than starting a new turn. Drained by
    /// `run_turn_loop` before each `build_completion_request` and at the EndTurn
    /// break, so the steer lands after the previous round's tool results
    /// (preserving `tool_use`→`tool_result` pairing) and the model sees it on
    /// its next round.
    pub fn enqueue_steer(
        &mut self,
        content: Vec<MessageContent>,
        ui: Option<crate::MessageUiMetadata>,
        cx: &mut Context<Self>,
    ) -> String {
        let mut message = Message::user_with_content(content);
        message.ui = ui;
        let id = message.id.clone();
        self.pending_steer.push_back(message);
        cx.notify();
        id
    }

    /// Ids of steer follow-ups still parked in the queue (not yet drained).
    /// Read at terminal `Stop`/`Error` so the UI can mark the matching parked
    /// queue cards as failed (stranded — the running turn exited without
    /// absorbing them) and offer a retry rather than leaving them spinning.
    pub fn pending_steer_ids(&self) -> Vec<String> {
        self.pending_steer.iter().map(|m| m.id.clone()).collect()
    }

    /// Drop a single queued steer follow-up by message id. Cancelling the UI
    /// card for a `SteerPending` steer must also pull the message out of the
    /// thread's steer queue — otherwise the running turn drains it anyway and
    /// feeds it to the model with no matching UI card, so the steer takes
    /// effect invisibly (the user clicked "remove" but it still went through).
    /// Returns whether a message was actually removed.
    pub fn cancel_pending_steer(&mut self, id: &str) -> bool {
        if let Some(pos) = self.pending_steer.iter().position(|m| m.id == id) {
            self.pending_steer.remove(pos);
            true
        } else {
            false
        }
    }

    /// Flush every queued steer follow-up onto `messages` in submission order.
    /// Returns whether any message was drained, so the caller can decide to
    /// continue the turn loop rather than ending on an unconsumed steer.
    /// Each drained message is tagged `steered` (marking it as a true
    /// mid-turn injection) and announced via `ThreadEvent::SteerInjected` so
    /// the UI moves the parked queue card into the conversation at the exact
    /// stream position the model will next see it.
    fn drain_pending_steer(&mut self, cx: &mut Context<Self>) -> bool {
        let mut drained = false;
        while let Some(mut msg) = self.pending_steer.pop_front() {
            // Tag at drain time — only messages the running turn actually
            // absorbs count as steered; enqueued-but-never-drained stragglers
            // stay untagged so the historical replay distinguishes a true steer
            // from an ordinary follow-up turn.
            let ui = msg.ui.get_or_insert_with(Default::default);
            ui.steered = Some(true);
            let id = msg.id.clone();
            self.messages.push(msg);
            cx.emit(ThreadEvent::SteerInjected { message_id: id });
            drained = true;
        }
        drained
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

    /// Start a skill turn: inject the named skill's body (with the user's args
    /// appended) as a user message and run. Mirrors `submit_command` but for
    /// skills — skills carry no `allowed-tools` filter or `$ARGUMENTS` template,
    /// so the body is emitted verbatim (description prefix + body, matching the
    /// `skill` tool's output) with `args` appended as the turn's task.
    pub fn submit_skill(&mut self, key: &str, args: &str, cx: &mut Context<Self>) -> bool {
        let Some(s) = crate::skill::global().get(key).cloned() else {
            return false;
        };
        let rendered = crate::prompt::render(
            crate::prompt::PromptTemplate::SkillBody,
            &crate::prompt::SkillBodyData {
                description: (!s.description.is_empty()).then(|| s.description.clone()),
                body: s.body.clone(),
                arguments: (!args.is_empty()).then(|| args.to_string()),
            },
        )
        .expect("skill body render");
        self.insert_user_message(rendered, cx);
        self.run_turn(cx);
        true
    }
    pub fn run_turn(&mut self, cx: &mut Context<Self>) {
        if self.running_turn.is_some() {
            return;
        }
        let Some(model) = self.active_model() else {
            cx.emit(ThreadEvent::Error(anyhow::anyhow!("No model configured")));
            return;
        };

        // Signal the UI immediately that a turn is in flight — before the
        // first streaming delta arrives — so the sidebar running indicator
        // lights up during the warm-up gap. `ThreadStore::set_running` (called
        // by the workspace on this event) is the bridge to the sidebar.
        self.turn_started_at = Some(Instant::now());
        cx.emit(ThreadEvent::TurnStarted);

        let cancel = CancellationToken::new();
        self.turn_cancel = Some(cancel.clone());

        let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = Self::run_turn_loop(&this, &model, &cancel, cx).await;
            let turn_failed = result.is_err();
            if let Err(e) = result {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(ThreadEvent::Error(e));
                });
            }
            let turn_cancelled = this
                .update(cx, |this, cx| {
                    this.running_turn = None;
                    this.turn_started_at = None;
                    this.turn_cancel = None;
                    // A slash command's tool filter lasts only for its turn; clear it
                    // so a subsequent free-form message inherits the full tool set.
                    this.turn_tool_filter = None;
                    // The turn is now truly over (the loop only returns once the
                    // EndTurn-break's drain guard sees an empty queue, or on
                    // cancel/error/max-turns). Any steer still queued here was
                    // never absorbed — it is stranded. Drop it so a later turn
                    // does not silently drain it into the model; the UI already
                    // marks the matching optimistic bubbles `Failed` from the
                    // `TurnFinished` payload, and the user retries there.
                    let stranded_steer_ids = this.pending_steer_ids();
                    this.pending_steer.clear();
                    // A natural (non-cancelled) turn end is the trigger point for
                    // title re-evaluation. `maybe_generate_title` self-gates on
                    // depth, cadence, and dedup; cancelled turns skip it.
                    if !cancel.is_cancelled() {
                        this.maybe_generate_title(cx);
                    }
                    let cancelled = cancel.is_cancelled();
                    cx.emit(ThreadEvent::TurnFinished {
                        cancelled,
                        failed: turn_failed,
                        stranded_steer_ids: stranded_steer_ids.clone(),
                    });
                    cx.notify();
                    cancelled
                })
                .ok()
                .unwrap_or(true);
            // Persistence + running-indicator backstop: `Stop`/`Error` events
            // are the workspace's signal to save the thread and clear the
            // sidebar running indicator, but a stream that ends without a
            // `MessageStop` (provider hiccup, non-SSE response from a
            // compatibility endpoint) makes run_turn_loop return Ok with no
            // terminal event emitted, so the workspace never learns the turn
            // ended — the assistant content is lost on the next thread switch
            // and the sidebar shimmer stays lit on the row. Persist and clear
            // the indicator here unconditionally; both are idempotent alongside
            // any overlapping event-driven save/clear, and a dropped entity
            // (the workspace already switched away) just skips.
            let successor_running = if let Some(entity) = this.upgrade() {
                cx.update(|cx: &mut gpui::App| {
                    let thread_id = entity.read(cx).id.0.clone();
                    let successor_running = entity.read(cx).is_running();
                    crate::thread_store::save_thread(entity, true, cx);
                    if !successor_running {
                        let store = crate::thread_store::global();
                        store.update(cx, |s, cx| s.mark_idle(&thread_id, cx));
                    }
                    successor_running
                })
            } else {
                // A dropped entity cannot continue a goal and needs no store
                // update. Treat it like a successor so both paths are skipped.
                true
            };
            // Goal auto-continuation: on a natural (non-cancelled) turn end
            // with an active goal, run the evaluator and either continue or
            // clear. A cancelled turn (user stop / `/goal clear` during a turn)
            // skips this so cancel is terminal. Sub-agents never carry a goal.
            if !turn_cancelled && !successor_running {
                Self::maybe_continue_goal(&this, &model, cx).await;
            }
        });

        self.running_turn = Some(task);
        cx.notify();
    }

    /// Hard cap on goal evaluations. Bounds the auto-continue loop when a
    /// condition is genuinely unmeetable or the evaluator keeps failing. 50
    /// evaluations ≈ a long but bounded work session; the user can re-issue
    /// `/goal <condition>` to restart with a fresh count.
    const GOAL_MAX_EVALUATIONS: u32 = 50;

    /// After a natural turn end, if a goal is active, run the evaluator. On
    /// "satisfied" → clear the goal (exits mode). On "unsatisfied" → inject
    /// the condition as a directive and start a new turn. No-op when no goal
    /// is active, on a sub-agent, or when an evaluator is somehow already in
    /// flight. The continuation runs in a separate spawned task so it does
    /// not hold `this`'s write lease across the call to `run_turn` (re-entrancy
    /// safety — `run_turn`'s `running_turn.is_some()` guard is the backstop).
    async fn maybe_continue_goal(
        this: &WeakEntity<Self>,
        model: &AnyLanguageModel,
        cx: &mut AsyncApp,
    ) {
        // Claim the evaluator slot and capture the condition. Bails when there
        // is no goal, this is a sub-agent, or an evaluator is already running.
        let claimed = this.update(cx, |this, cx| {
            if this.depth != 0 {
                return None;
            }
            // Plan turns do not advance a goal: codex gates `account_tokens` on
            // `!Plan`, and a Plan turn is read-only research with no execution
            // to evaluate progress against. Goal and Plan are also mutually
            // exclusive at entry, so this is a backstop for any stale path.
            if this.collaboration_mode == ModeKind::Plan {
                return None;
            }
            let g = this.goal.as_mut()?;
            if this.goal_cancel.is_some() {
                return None;
            }
            let cancel = CancellationToken::new();
            this.goal_cancel = Some(cancel.clone());
            g.evaluations += 1;
            let n = g.evaluations;
            cx.notify();
            Some((g.condition.clone(), n, cancel))
        });
        let Ok(Some((condition, eval_count, cancel))) = claimed else {
            return;
        };

        // Snapshot the message list for the evaluator under a short lease.
        let messages = this
            .read_with(cx, |this, _| this.messages.clone())
            .unwrap_or_default();

        let verdict: GoalVerdict = goal::evaluate(model, &condition, &messages, cancel, cx).await;

        // Write the verdict back and decide continue vs. clear vs. abort.
        let action = this
            .update(cx, |this, cx| {
                this.goal_cancel = None;
                // The goal was cleared while the evaluator was in flight (user
                // hit `/goal clear` or cancel) — drop the result silently.
                let Some(g) = this.goal.as_mut() else {
                    return GoalAction::Abort;
                };
                g.last_reason = verdict.reason.clone();
                cx.emit(ThreadEvent::GoalEvaluated {
                    satisfied: verdict.satisfied,
                    reason: verdict.reason,
                    evaluations: eval_count,
                });
                // Cap: too many evaluations without satisfaction → clear and
                // surface the abort in the last reason, rather than looping
                // forever.
                if eval_count >= Self::GOAL_MAX_EVALUATIONS {
                    g.last_reason = format!(
                        "goal aborted after {} evaluations: {}",
                        eval_count, g.last_reason
                    );
                    return GoalAction::Abort;
                }
                if verdict.satisfied {
                    GoalAction::Satisfied
                } else {
                    GoalAction::Continue
                }
            })
            .ok();
        let Some(action) = action else {
            return;
        };

        match action {
            GoalAction::Abort | GoalAction::Satisfied => {
                // Satisfied → clear goal (exits mode, emits GoalChanged{false}).
                // Abort (cap hit or goal cleared mid-eval) → clear too; if the
                // goal was already cleared this is a no-op.
                let _ = this.update(cx, |this, cx| this.clear_goal(cx));
            }
            GoalAction::Continue => {
                // Inject the condition as the next turn's directive and start
                // the turn in a separate task so we do not re-enter `run_turn`
                // while still inside this callback's `this.update` callers. The
                // goal may have been cleared in the window between the verdict
                // decision and this spawned task running — re-check so a late
                // `/goal clear` does not kick off a spurious continuation turn.
                let this = this.clone();
                cx.spawn(async move |cx: &mut AsyncApp| {
                    let _ = this.update(cx, |this, cx| {
                        if this.goal.is_none() {
                            return;
                        }
                        let directive = crate::prompt::render(
                            crate::prompt::PromptTemplate::WrapperGoalContinuation,
                            &crate::prompt::GoalContinuationData {
                                condition: condition.clone(),
                            },
                        )
                        .expect("goal continuation template render");
                        this.insert_user_message(directive, cx);
                        this.run_turn(cx);
                    });
                })
                .detach();
            }
        }
    }

    /// The index at which a compaction should be inserted right now under
    /// auto-trigger rules, or `None` when auto-compaction is off, the window is
    /// too small, a compaction already covers the region, or the threshold has
    /// not been crossed. Sub-agents never auto-compact — a delegated
    /// sub-thread should not silently rewrite its own context under the
    /// parent's nose (its overflow rescue path is separate and unconditional).
    ///
    /// The trigger fires when the effective context fill — the larger of the
    /// provider-reported usage and a local bytes/4 estimate of the live
    /// history — crosses `max_input * threshold_pct` (see
    /// `compact::effective_context_tokens`).
    fn auto_compaction_target(&self) -> Option<usize> {
        if self.depth != 0 {
            return None;
        }
        let settings = crate::settings::load();
        if !settings.auto_compact.enabled {
            return None;
        }
        let model = self.model.as_ref()?;
        // Claude Code parity: an explicit auto-compact window from the
        // provider config's `CLAUDE_CODE_AUTO_COMPACT_WINDOW` env var drives
        // both the window and an 80% trigger threshold. Without it, fall back
        // to the model's full `max_token_count` at the user's settings
        // threshold (default 0.8). `settings.auto_compact.enabled` still gates
        // the whole mechanism.
        let (max_input, threshold) = match model.auto_compact_window() {
            Some(window) => (window, crate::compact::CLAUDE_CODE_AUTO_COMPACT_THRESHOLD),
            None => (model.max_token_count(), settings.auto_compact.threshold),
        };
        crate::compact::auto_compaction_target_ix(
            &self.messages,
            self.token_meter.per_request(),
            settings.auto_compact.enabled,
            max_input,
            threshold,
        )
    }

    /// Manually trigger a compaction (`/compact`). No-op when a turn is in
    /// flight (the turn owns the message list), when there is no model, or when
    /// there is nothing to summarize. Runs the side LLM call in a spawned task
    /// so the call site returns immediately.
    pub fn compact(&mut self, cx: &mut Context<Self>) {
        if self.running_turn.is_some() {
            return;
        }
        let Some(model) = self.model.clone() else {
            cx.emit(ThreadEvent::Error(anyhow::anyhow!("No model configured")));
            return;
        };
        let Some(insertion_ix) = crate::compact::forced_compaction_target_ix(&self.messages) else {
            return;
        };
        // A manual compaction is a fresh side call, not bound to the turn
        // lifecycle; a fresh (never-cancelled) token suffices. A stuck call is
        // cleared by closing the thread for now — wiring it into `cancel()` is
        // left for a follow-up.
        let cancel = CancellationToken::new();
        cx.notify();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let outcome = Self::perform_compaction(&this, &model, insertion_ix, &cancel, cx).await;
            if let Err(e) = outcome {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(ThreadEvent::Error(e));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Run the side LLM call that summarizes `messages[0..insertion_ix]` into a
    /// handoff summary, then insert a `Compaction` message at `insertion_ix`,
    /// emit `ThreadEvent::CompactionStarted` (before the call) then
    /// `ThreadEvent::Compaction` (after), persist, record the event, and bust
    /// the prefix-cache fingerprint. The summary request is built and streamed
    /// outside any `this` write lease; only the final insert + bookkeeping hold
    /// the lease. Returns the summary text on success.
    /// The overflow recovery shared by the handshake and mid-stream failure
    /// paths: compact history once so the re-request can fit the window.
    /// Returns `false` when there is nothing left to compact — the caller
    /// then aborts the turn with guidance. Unlike threshold auto-compaction
    /// this rescue runs regardless of `settings.auto_compact.enabled` and of
    /// `depth`: a sub-agent drowning in oversized tool results needs the
    /// same lifeline as the main thread.
    async fn compact_for_overflow_retry(
        this: &gpui::WeakEntity<Self>,
        model: &AnyLanguageModel,
        cancel: &CancellationToken,
        cx: &mut AsyncApp,
    ) -> Result<bool> {
        let insertion_ix = this.read_with(cx, |this, _| {
            crate::compact::forced_compaction_target_ix(&this.messages)
        })?;
        let Some(insertion_ix) = insertion_ix else {
            return Ok(false);
        };
        Self::perform_compaction(this, model, insertion_ix, cancel, cx).await?;
        Ok(true)
    }

    async fn perform_compaction(
        this: &WeakEntity<Self>,
        model: &AnyLanguageModel,
        insertion_ix: usize,
        cancel: &CancellationToken,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let tokens_before = this
            .read_with(cx, |this, _| {
                this.token_meter
                    .per_request()
                    .values()
                    .map(|u| crate::compact::active_tokens(*u))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        // Signal the cockpit that the side-LLM summarization is in flight so
        // it can flip to a "Summarizing context…" phase; the matching
        // `Compaction` event below reverts it. Emitted under its own short
        // lease — never held across the `stream_summary` await.
        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::CompactionStarted { tokens_before });
            cx.notify();
        })?;
        let request = this.read_with(cx, |this, _| {
            crate::compact::build_compaction_request(&this.messages, insertion_ix)
        })?;
        let (summary, usage) =
            crate::compact::stream_summary(model, request, cancel.clone(), cx).await?;

        let messages_compacted = this
            .read_with(cx, |this, _| insertion_ix.min(this.messages.len()))
            .unwrap_or(insertion_ix);
        this.update(cx, |this, cx| {
            // Attribute the side call's tokens to the cumulative + per-model
            // meter only — no per-user-message attribution, and never touch
            // `current_request` (the call has no triggering user message, so
            // stamping its tokens there would let the next `finalize_request`
            // mis-attribute them to the following user message). Emitted so the
            // UI counter reflects the spend.
            if let Some(u) = usage {
                if let Some(model) = this.model.as_ref() {
                    this.token_meter
                        .accumulate_side_call_usage(u, &model.name());
                }
                cx.emit(ThreadEvent::TokenUsageUpdated(
                    this.cumulative_token_usage(),
                ));
            }
            let compaction_msg =
                Message::user_with_content(vec![MessageContent::Compaction(summary.clone())]);
            this.messages.insert(insertion_ix, compaction_msg);
            // The summary may absorb the lazy instruction reminders injected
            // into earlier tool results; clearing the injected set lets a
            // re-read of the same directory re-inject them (matching Claude
            // Code, which reloads nested CLAUDE.md on the next read after a
            // compaction).
            this.lazy_instructions_injected.clear();
            // A compaction rewrites the message prefix, so the provider's KV
            // cache misses once on the next request. That is the unavoidable
            // cost of reclaiming context; invalidate the fingerprint so the
            // stability diagnostic reports a deliberate reset rather than a
            // silent drift.
            this.prefix_stability.invalidate();
            cx.emit(ThreadEvent::Compaction {
                summary: summary.clone(),
                messages_compacted,
                tokens_before,
            });
            cx.notify();
        })?;

        // Persist the compacted thread and record a `compaction` event in the
        // timeline, mirroring the model-change persistence pattern.
        if let Some(entity) = this.upgrade() {
            cx.update(|cx: &mut gpui::App| {
                let thread_id = entity.read(cx).id.0.clone();
                crate::thread_store::save_thread(entity, true, cx);
                let store = crate::thread_store::global();
                let data = serde_json::json!({
                    "messages_compacted": messages_compacted,
                    "tokens_before": tokens_before,
                });
                store.update(cx, |s, cx| {
                    s.record_event(
                        &thread_id,
                        crate::db::ThreadEventType::Compaction,
                        &data,
                        cx,
                    );
                });
            });
        }
        Ok(summary)
    }

    /// Abort the current turn. Cancels the turn token so an in-flight tool (e.g.
    /// `bash`) can kill its process group and append a clean "aborted" result;
    /// the turn task then winds down on its own and clears `running_turn`.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.turn_cancel.take() {
            cancel.cancel();
            // A cancelled slash-command turn must also clear its tool filter.
            self.turn_tool_filter = None;
            // Symmetric cleanup of pending tool-authorization oneshots. A
            // cancelled turn resolves in-flight approvals via the cancellation
            // token's `select!` arms, but the senders must also be dropped so a
            // late `respond_authorization` from the UI silently no-ops instead
            // of panicking on a closed channel.
            self.pending_authorizations.clear();
            self.pending_auth_meta.clear();
            // Inbound-write oneshots are not turn-scoped (a page can request a
            // write at any time), but a cancel still drops them so a late
            // `respond_inbound` no-ops rather than panicking on a closed
            // channel. A fresh request re-registers a fresh oneshot.
            self.pending_inbound.clear();
            // Drop bubbled sub-agent auth routes too: a parent cancel must
            // unwind child-authorization routing so a late composite-id
            // response no-ops at the parent instead of traversing to a child
            // whose own pending map is already empty.
            self.pending_child_auth.clear();
            // Abort any in-flight goal evaluator so a late verdict does not
            // trigger an auto-continuation after the user cancelled. The goal
            // itself is left in place — cancel stops the current turn, it does
            // not discard the condition (the user can `/goal` to inspect or
            // re-submit). `maybe_continue_goal` is gated on
            // `cancel.is_cancelled()`, so a cancelled turn never reaches the
            // evaluator entry point either.
            if let Some(c) = self.goal_cancel.take() {
                c.cancel();
            }
            // Attribute partial usage from the cancelled turn and reset the
            // per-request counter so the next turn's delta starts from zero.
            self.finalize_request_usage();
            cx.emit(ThreadEvent::Stop(StopReason::Cancelled));
            cx.notify();
        }

        // A leader cancel propagates to every worker member: their in-flight
        // turns are part of the same coordinated effort, so stopping the leader
        // must not leave members running. Only the main thread (depth 0) is a
        // team leader — a worker's own Stop must not cascade to siblings.
        // Collect the handles first, then update — `Entity::update` needs
        // `&mut App`, which a live `team.read` guard would block. A member's own
        // `cancel` unwinds its nested sub-agent turns via its `turn_cancel`
        // select! arms, so one level suffices.
        if self.depth == 0
            && let Some(team) = self.team.clone()
        {
            let members: Vec<gpui::Entity<Thread>> = team
                .read(cx)
                .members()
                .values()
                .map(|m| m.thread().clone())
                .collect();
            for m in members {
                m.update(cx, |t, cx| t.cancel(cx));
            }
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
            // Reset the `<proposed_plan>` parser for the turn: a prior turn's
            // partial block state must not bleed into this one.
            this.proposed_plan_parser = ProposedPlanParser::new();
            this.pending_plan_text.clear();
            this.produced_plan_this_turn = false;
            // Reset the turn's recovery state: a prior turn's parse error or
            // max-tokens truncation must not bleed into this one. The retry
            // counter is turn-scoped — it accumulates across this turn's
            // recovery round-trips, then resets here on the next user turn.
            this.pending_parse_error = false;
            this.pending_stream_error = None;
            this.pending_context_overflow = false;
            this.recovery_retries = 0;
            this.overflow_recovery_attempted = false;
            this.last_stop_reason = None;
        })?;
        loop {
            // Increment at the top so `turn_count` is 1-indexed for the
            // round-trip about to run — `self_info` mid-batch reads the current
            // index, not the previous batch's count. Also clear the round-
            // trip's recovery flags: a previous round-trip's stop reason or
            // parse-error flag has either been consumed by the empty-tool-use
            // branch or is stale (the model produced executable tools this
            // round, so no recovery path applies).
            this.update(cx, |this, _| {
                this.turn_count += 1;
                this.pending_parse_error = false;
                this.pending_stream_error = None;
                this.pending_context_overflow = false;
                this.last_stop_reason = None;
            })?;

            // Auto-compaction: if the previous request's token usage crossed
            // the threshold, summarize the old history into a handoff message
            // before building this turn's request. Sub-agents never auto-
            // compact (`auto_compaction_target` enforces depth 0). Runs inline
            // so the compaction is in place before `build_completion_request`
            // assembles the request; a failure here surfaces as a turn error
            // rather than a silent skip.
            let target = this
                .read_with(cx, |this, _| this.auto_compaction_target())
                .unwrap_or(None);
            if let Some(insertion_ix) = target {
                Self::perform_compaction(this, model, insertion_ix, cancel, cx).await?;
            }

            let request = this.update(cx, |this, cx| {
                this.pending_tool_uses.clear();
                // Pair any orphaned tool_uses (truncated MaxTokens rounds,
                // interrupted sessions) with error tool_results *before*
                // draining a steer — otherwise the steer user message lands
                // last and `reconcile_tool_uses` skips the pairing, shipping an
                // unpaired `tool_use` to the provider (Anthropic 400).
                this.reconcile_tool_uses(cx);
                this.drain_pending_steer(cx);
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
                // A handshake failure (network drop after retries, auth error,
                // etc.) does not end the turn dead: no event was ever produced,
                // so `tool_uses` stays empty and the empty-tool-use branch below
                // picks up `pending_stream_error` to nudge the model to retry or
                // wrap up — mirroring the parse-error / max-tokens recovery, and
                // capped by the same per-turn retry budget (thread 0b80401d).
                s = model.stream_completion(request, cx) => match s {
                    Ok(s) => s,
                    Err(e) => {
                        // A context-overflow rejection is deterministic: the
                        // identical request can never succeed, so skip the
                        // nudge-and-retry and spend the turn's single
                        // compact-and-retry instead.
                        if e.downcast_ref::<crate::provider::overflow::ContextOverflow>().is_some() {
                            let already_attempted = this.update(cx, |this, _| {
                                std::mem::replace(&mut this.overflow_recovery_attempted, true)
                            })?;
                            if already_attempted {
                                return Err(anyhow::anyhow!(
                                    "context overflow: the request still exceeds the model's \
                                     input limit after one compaction retry. Run /compact or \
                                     start a new thread. Last error: {e}"
                                ));
                            }
                            if !Self::compact_for_overflow_retry(this, model, cancel, cx).await? {
                                return Err(anyhow::anyhow!(
                                    "context overflow: nothing left to compact. Start a new thread."
                                ));
                            }
                            continue;
                        }
                        let abort_err = this.update(cx, |this, cx| {
                            this.recovery_retries += 1;
                            let round = this.turn_count;
                            if this.recovery_retries > MAX_RECOVERY_ATTEMPTS {
                                tracing::warn!(
                                    round,
                                    error = %e,
                                    kind = "handshake",
                                    "stream_completion failed; recovery retries exhausted, aborting turn"
                                );
                                this.last_stop_reason = None;
                                Some(anyhow::anyhow!(
                                    "turn aborted: exceeded {MAX_RECOVERY_ATTEMPTS} recovery retries (stream errors); last error: {e}"
                                ))
                            } else {
                                tracing::warn!(
                                    round,
                                    error = %e,
                                    kind = "handshake",
                                    "stream_completion failed; recovery nudge injected"
                                );
                                // The handshake produced no event, so no
                                // assistant message started and the empty-tool-
                                // use branch below never runs this round. Inject
                                // the nudge directly here so the next round-trip
                                // carries it (append-only, prefix cache intact).
                                this.insert_user_message(
                                    format!(
                                        "The previous model request failed: {e}. \
                                         Retry the last step, or summarize the current progress \
                                         and deliver the result if the work is done."
                                    ),
                                    cx,
                                );
                                this.last_stop_reason = None;
                                None
                            }
                        })?;
                        if let Some(err) = abort_err {
                            return Err(err);
                        }
                        continue;
                    }
                },
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
            // A round that streamed without an error proves the request was
            // sendable: any overflow recovery spent earlier has done its job,
            // so a later, genuinely-new overflow earns its own single retry.
            this.update(cx, |this, _| {
                if this.pending_stream_error.is_none() {
                    this.overflow_recovery_attempted = false;
                }
            })?;

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

                    if !requires_approval || always_allowed {
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
                // The model stopped without producing any executable tool call.
                // Five cases: a clean EndTurn (the turn is done),
                // max_output_tokens truncation (the response was cut mid-stream,
                // often mid-tool_use), a tool_use JSON parse error (the
                // placeholder tool_use + error tool_result are already in
                // history), a degenerate empty turn (only empty Thinking, no
                // visible text — thread 1c9c8df1 msg23), or an unfulfilled
                // tool intent (visible text *announcing* a tool action but no
                // tool_use emitted — thread 480b2469 msg3/5/9). The last four
                // feed a failure/nudge back to the model and continue the loop
                // — aligning with pi/oh-my-pi/codex/zed — instead of ending the
                // turn dead on a half-finished or empty assistant message.
                // Append-only: a directive, an error tool_result, or a user
                // nudge is added, history is never rewritten, so the provider
                // prefix cache stays intact. All four recovery paths share a
                // per-turn retry cap so a model stuck re-truncating,
                // re-emitting bad JSON, or re-producing empty/unfulfilled turns
                // cannot loop forever (the main thread has no `max_turns`
                // guard).
                let recovery = this.update(cx, |this, cx| {
                    let parse_err = this.pending_parse_error;
                    // `take()` so the flag does not outlive this recovery: a
                    // nudge is appended once, and the flag must not bleed into a
                    // later round-trip that already produced executable tools.
                    let stream_err = this.pending_stream_error.take();
                    let max_tok = this.last_stop_reason == Some(StopReason::MaxTokens);
                    // Degenerate empty turn: no tool_use and no visible
                    // assistant text (only empty Thinking). Distinct from
                    // parse_err / stream_err / max_tok — the model produced
                    // nothing useful and the turn would otherwise end silently.
                    let last_assistant_text = match this.messages.last() {
                        Some(m) if m.role == Role::Assistant => m
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                MessageContent::Text(t) => Some(t.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                        _ => String::new(),
                    };
                    let degenerate = !parse_err
                        && stream_err.is_none()
                        && !max_tok
                        && !this.produced_plan_this_turn
                        && last_assistant_text.trim().is_empty();
                    // Unfulfilled tool intent: the turn has visible text (so not
                    // degenerate) that announces a tool action — "let me search",
                    // "让我先找到" — yet no tool_use was emitted, so nothing
                    // happened. The degenerate check cannot reach this case (it
                    // requires *no* visible text); without it the model loops
                    // "narrate intent → clean EndTurn" indefinitely.
                    let unfulfilled_intent = !parse_err
                        && stream_err.is_none()
                        && !max_tok
                        && !degenerate
                        && this.collaboration_mode != ModeKind::Plan
                        && announces_tool_intent_without_call(&last_assistant_text);
                    if this.produced_plan_this_turn
                        || (!parse_err
                            && stream_err.is_none()
                            && !max_tok
                            && !degenerate
                            && !unfulfilled_intent)
                    {
                        return RecoveryAction::Done;
                    }
                    // A context-overflow stream error is deterministic: the
                    // identical request can never succeed, so it neither
                    // consumes the generic retry budget nor gets a nudge —
                    // compact once and retry. A second overflow after the
                    // recovery aborts the turn with guidance.
                    if stream_err.is_some() && this.pending_context_overflow {
                        this.pending_context_overflow = false;
                        this.last_stop_reason = None;
                        if this.overflow_recovery_attempted {
                            cx.emit(ThreadEvent::Error(anyhow::anyhow!(
                                "context overflow: the request still exceeds the model's \
                                 input limit after one compaction retry. Run /compact or \
                                 start a new thread."
                            )));
                            return RecoveryAction::Abort;
                        }
                        this.overflow_recovery_attempted = true;
                        return RecoveryAction::CompactAndRetry;
                    }
                    this.recovery_retries += 1;
                    if this.recovery_retries > MAX_RECOVERY_ATTEMPTS {
                        cx.emit(ThreadEvent::Error(anyhow::anyhow!(
                            "turn aborted: exceeded {MAX_RECOVERY_ATTEMPTS} recovery retries (stream error / tool_use JSON parse error / max_output_tokens truncation / degenerate empty turn / unfulfilled tool intent)"
                        )));
                        this.pending_parse_error = false;
                        this.pending_stream_error = None;
                        this.last_stop_reason = None;
                        return RecoveryAction::Abort;
                    }
                    if parse_err {
                        // The error tool_result is already in history from
                        // `handle_completion_event`; the model retries from it.
                        // Nothing to append.
                    } else if let Some(reason) = stream_err {
                        // Mid-stream failure: the assistant message (if any) is
                        // half-finished. Nudge the model to retry the last step
                        // or wrap up and deliver, so a transient transport
                        // failure does not end the turn mid-work. Handshake
                        // failures never reach here — they nudge at the call
                        // site, before any assistant message starts.
                        this.insert_user_message(
                            crate::prompt::render(
                                crate::prompt::PromptTemplate::WrapperRecoveryFailure,
                                &crate::prompt::RecoveryFailureData {
                                    reason: reason.to_string(),
                                },
                            )
                            .expect("recovery failure template render"),
                            cx,
                        );
                    } else if max_tok {
                        // MaxTokens: append a "redo compactly" directive so the
                        // model shortens the next attempt to fit the budget.
                        this.append_max_tokens_directive(cx);
                    } else if unfulfilled_intent {
                        // The model announced a tool action but emitted no
                        // tool_use. Nudge it to either call the tool now or
                        // deliver an answer with no action announcement —
                        // breaking the narrate-without-acting loop.
                        this.insert_user_message(
                            crate::prompt::render_static(
                                crate::prompt::PromptTemplate::WrapperUnfulfilledToolIntentNudge,
                            )
                            .expect("unfulfilled tool intent nudge render"),
                            cx,
                        );
                    } else {
                        // Degenerate empty turn: nudge the model toward visible
                        // output so it does not end silently on empty reasoning.
                        this.insert_user_message(
                            crate::prompt::render(
                                crate::prompt::PromptTemplate::WrapperEmptyTurnNudge,
                                &crate::prompt::EmptyTurnNudgeData {
                                    in_plan: this.collaboration_mode == ModeKind::Plan,
                                },
                            )
                            .expect("empty turn nudge render"),
                            cx,
                        );
                    }
                    this.pending_parse_error = false;
                    this.pending_stream_error = None;
                    this.last_stop_reason = None;
                    RecoveryAction::Continue
                })?;
                match recovery {
                    RecoveryAction::Done => {
                        // A steer follow-up queued during the final assistant
                        // text is not yet drained; loop once more so the next
                        // build absorbs it instead of orphaning the user
                        // message. The drain at the top of the loop flushes it
                        // onto `messages` after the last round's tool results.
                        let steer_pending =
                            this.update(cx, |this, _cx| !this.pending_steer.is_empty())?;
                        if steer_pending {
                            continue;
                        }
                        break;
                    }
                    RecoveryAction::Abort => break,
                    RecoveryAction::Continue => continue,
                    RecoveryAction::CompactAndRetry => {
                        let compacted = match model.as_ref() {
                            Some(model) => {
                                Self::compact_for_overflow_retry(this, model, cancel, cx).await?
                            }
                            None => false,
                        };
                        if !compacted {
                            this.update(cx, |_, cx| {
                                cx.emit(ThreadEvent::Error(anyhow::anyhow!(
                                    "context overflow: nothing left to compact. Start a new thread."
                                )))
                            })?;
                            break;
                        }
                        continue;
                    }
                }
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
                        crate::prompt::render(
                            crate::prompt::PromptTemplate::WrapperMaxTurnsSummary,
                            &crate::prompt::MaxTurnsSummaryData { max },
                        )
                        .expect("max turns summary render"),
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
            let msg = format!("Unknown tool: {name}");
            Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
            Self::append_tool_result(&this, tu, msg.clone(), true, cx)?;
            return Ok(());
        };

        // Plan-mode write backstop: a write tool should never be advertised in
        // plan mode (the request-tool list is filtered), but if one slips
        // through (stale registry, model hallucination) synthesize an error
        // rather than execute it.
        let in_plan = this.read_with(cx, |this, _| this.collaboration_mode == ModeKind::Plan)?;
        if in_plan && !tool.is_read_only() {
            let msg = format!("Plan mode does not permit calling {name}.");
            Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
            Self::append_tool_result(&this, tu, msg, true, cx)?;
            return Ok(());
        }

        // YOLO/AutoReview bypasses the permission gate: skip the authorization
        // prompt entirely so the tool runs immediately. YOLO is the
        // session-level "never ask" policy;
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
                    input: Some(tu.input.clone()),
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
                    let msg = crate::prompt::render_static(
                        crate::prompt::PromptTemplate::WrapperToolDenied,
                    )
                    .expect("tool denied render");
                    Self::emit_tool_result(&this, &id, &name, &title, &msg, true, cx)?;
                    Self::append_tool_result(&this, tu, msg, true, cx)?;
                    return Ok(());
                }
                ToolAuthorizationResponse::Decision(PermissionDecision::AlwaysAllow) => {
                    this.read_with(cx, |this, _| this.permission.set_always_allowed(&name))?;
                }
                ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce) => {}
                ToolAuthorizationResponse::AskUserQuestion { answers, response } => {
                    let output = crate::prompt::render(
                        crate::prompt::PromptTemplate::WrapperAskUserQuestions,
                        &crate::prompt::AskUserQuestionsData {
                            answers: answers
                                .into_iter()
                                .map(|(q, a)| crate::prompt::AskUserQa {
                                    question: q,
                                    answer: a,
                                })
                                .collect(),
                            response,
                        },
                    )
                    .expect("ask user questions render");
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
                input: Some(tu.input.clone()),
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
        // Ensure the thread's in-process network proxy is running before
        // building the ToolContextSnapshot. The proxy is spawned lazily on
        // the first sandboxed bash call when the network policy is
        // `Restricted`; the snapshot captures its port so the bash tool
        // can inject it into `wrap_command`. `None` for `Blocked` /
        // `Unrestricted` (no proxy needed). This must happen before
        // `from_thread` so the snapshot sees a live proxy port.
        this.update(cx, |t, _| t.ensure_network_proxy())?;

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
        // Every tool result enters the conversation bounded: the UI event, the
        // persisted messages, and the PostToolUse hook all share this one
        // capped text. The `agent` envelope is exempt — it is bounded at
        // construction and the UI parses it as JSON, which truncation would
        // corrupt.
        let output_str = if tu.name.as_ref() == crate::tools::AGENT {
            output_str
        } else {
            crate::tools::truncate::truncate_result(&output_str).into_owned()
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
                input: Some(tu.input.clone()),
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
        let msg = "Tool not executed (session cancelled)".to_string();
        this.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                title: title.clone(),
                status: ToolCallStatus::Denied,
                input: Some(tu.input.clone()),
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
                // Route the delta through the `<proposed_plan>` parser: only
                // visible text enters the assistant message (and thus the next
                // request's context); block content becomes a live plan delta.
                for segment in self.proposed_plan_parser.feed(&text) {
                    match segment {
                        ProposedPlanSegment::Normal(t) => {
                            self.append_assistant_text(t.clone(), cx);
                            cx.emit(ThreadEvent::AgentText(t));
                        }
                        ProposedPlanSegment::Delta(t) => {
                            self.pending_plan_text.push_str(&t);
                            self.produced_plan_this_turn = true;
                            cx.emit(ThreadEvent::PlanDelta { delta: t });
                        }
                        ProposedPlanSegment::Start | ProposedPlanSegment::End => {}
                    }
                }
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
                // Flush the parser's tail and close any block left open at
                // end-of-turn. Trailing visible text enters the assistant
                // message; remaining block content joins the plan accumulator.
                for segment in self.proposed_plan_parser.finish() {
                    match segment {
                        ProposedPlanSegment::Normal(t) => {
                            self.append_assistant_text(t.clone(), cx);
                            cx.emit(ThreadEvent::AgentText(t));
                        }
                        ProposedPlanSegment::Delta(t) => {
                            self.pending_plan_text.push_str(&t);
                            self.produced_plan_this_turn = true;
                            cx.emit(ThreadEvent::PlanDelta { delta: t });
                        }
                        ProposedPlanSegment::Start | ProposedPlanSegment::End => {}
                    }
                }
                // A completed plan surfaces to the UI as a turn-end review
                // prompt — only on a terminal stop, never mid-turn (a
                // `StopReason::ToolUse` between research rounds must not pop the
                // review). Drain the accumulator so a next turn starts clean.
                if !matches!(reason, StopReason::ToolUse) && !self.pending_plan_text.is_empty() {
                    let plan_text = std::mem::take(&mut self.pending_plan_text);
                    cx.emit(ThreadEvent::PlanReady { plan_text });
                }
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
                self.last_stop_reason = Some(reason);
                cx.emit(ThreadEvent::Stop(reason));
            }
            Ok(LanguageModelCompletionEvent::Retry {
                attempt,
                max_attempts,
                delay_secs,
                reason,
                detail,
            }) => {
                cx.emit(ThreadEvent::Retry {
                    attempt,
                    max_attempts,
                    delay_secs,
                    reason,
                    detail,
                });
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
                // Flag the round-trip so the empty-tool-use branch in
                // `run_turn_loop` continues the loop to feed this failure
                // back to the model, instead of ending the turn dead on the
                // orphaned error result (thread 76aef71a). The placeholder
                // tool_use is in the assistant message but deliberately not
                // enqueued for execution, so `pending_tool_uses` stays empty.
                self.pending_parse_error = true;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    kind = "mid-stream",
                    "completion stream error mid-turn; recovery nudge follows if no tool_use was produced"
                );
                // Flag the round-trip so the empty-tool-use branch nudges the
                // model to retry or wrap up, instead of ending the turn dead on
                // a half-finished assistant message (thread 0b80401d). When a
                // tool_use was already produced the flag is moot — the tool path
                // runs and the loop re-requests; it is cleared at round-trip top.
                // A classified overflow is remembered so the recovery compacts
                // once instead of nudging an unsendable request.
                self.pending_context_overflow = e
                    .downcast_ref::<crate::provider::overflow::ContextOverflow>()
                    .is_some();
                self.pending_stream_error = Some(format!("{e}"));
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
            let output = this.maybe_augment_read_instructions(&tu, output, is_error);
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

    /// The lazy half of the multi-level CLAUDE.md hierarchy: a successful
    /// `read_file` appends newly-discovered nested instruction files (and
    /// matching path-scoped rules) to its own tool result as a
    /// `<system-reminder>` — Claude Code's trigger semantics ("loads when
    /// Claude reads files in those directories"). The reminder rides the tool
    /// result rather than a standalone message: append-only history, no user
    /// bubble in the UI, and the prefix cache stays stable once written.
    /// `lazy_instructions_injected` dedups across reads; a compaction clears
    /// it so reminders the summary dropped re-inject on the next read.
    fn maybe_augment_read_instructions(
        &mut self,
        tu: &LanguageModelToolUse,
        output: String,
        is_error: bool,
    ) -> String {
        if tu.name.as_ref() != "read_file" || is_error || !self.instructions_enabled {
            return output;
        }
        let Some(raw_path) = tu.input.get("path").and_then(|v| v.as_str()) else {
            return output;
        };
        let (path_str, _selector) = crate::tools::path_selector::split_path_and_sel(raw_path);
        let read_path = if Path::new(path_str).is_absolute() {
            PathBuf::from(path_str)
        } else {
            self.cwd.join(path_str)
        };
        let ctx = crate::settings::claude_md_load_context();
        let set = crate::claude_md::load(&self.cwd, &ctx);
        let found = crate::claude_md::discover_for_read(
            &read_path,
            &self.cwd,
            &set.scoped,
            &self.lazy_instructions_injected,
            &ctx,
        );
        let Some(reminder) = crate::claude_md::render_lazy(&found) else {
            return output;
        };
        self.lazy_instructions_injected
            .extend(found.iter().map(|s| s.path.clone()));
        format!("{output}\n\n{reminder}")
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

    /// Append the max-tokens recovery directive (`system_prompt::max_tokens_directive`)
    /// as a `Text` block to the trailing user message, or to a fresh user
    /// message when the last message is the truncated assistant turn. The stop
    /// path finalizes the assistant message before this runs, so in practice
    /// the directive lands in a new user message — append-only, so the prefix
    /// cache is unaffected. Mirrors `push_tool_result`'s "accumulate into the
    /// last user message" pattern: a directive is just another user-role block
    /// paired with the preceding assistant turn.
    fn append_max_tokens_directive(&mut self, _cx: &mut Context<Self>) {
        let needs_new = match self.messages.last() {
            Some(m) => m.role != Role::User,
            None => true,
        };
        if needs_new {
            self.messages.push(Message::user_with_content(Vec::new()));
        }
        if let Some(m) = self.messages.last_mut() {
            m.push_content(MessageContent::Text(
                crate::prompt::render_static(
                    crate::prompt::PromptTemplate::WrapperMaxTokensDirective,
                )
                .expect("max tokens directive render"),
            ));
        }
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
        {
            normalize_assistant_content(&mut m.content);
            // A fully empty assistant message (only whitespace text/thinking
            // was streamed, all dropped above) is invalid on the provider wire
            // — keep one empty Text block so the message stays serializable.
            // The degenerate-empty recovery check still fires on it (empty
            // visible text), so this safety net does not mask the loop.
            if m.content.is_empty() {
                m.push_text(String::new());
            }
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
        // System message: a pre-rendered base plus optional mode addendums,
        // assembled through the `system/assembly` template so no `push_str` of
        // model prose lands in flow code. The base is the main-thread prompt
        // (built from live state) for the main thread, or the sub-agent's own
        // `agents/*.md` body for a sub-agent. Sub-agents additionally get the
        // language directive and (if worktree-isolated) a worktree section;
        // the main thread already bakes both into its identity block via
        // `build_main_system_prompt`.
        let wt = self
            .worktree
            .as_ref()
            .map(|w| (w.branch.as_str(), w.path.as_path()));
        let is_subagent = self.system.is_some();
        let base = match &self.system {
            Some(subagent_body) => subagent_body.clone(),
            None => crate::system_prompt::build_main_system_prompt(
                &self.cwd,
                self.project.as_deref(),
                self.approval_mode,
                wt,
            ),
        };
        // Capability ground truth (tools / images) comes from the resolved
        // model's operator-declared config fields, not model self-report — so
        // a non-multimodal model cannot claim image ability (thread 480b2469).
        // Model-stable, so the system prefix stays byte-stable across turns.
        let active_model = self.active_model();
        let active_effort = self.active_reasoning_effort();
        let capabilities = crate::prompt::ModelCapabilitiesPromptData {
            supports_tools: active_model.as_ref().is_none_or(|m| m.supports_tools()),
            supports_images: active_model.as_ref().is_some_and(|m| m.supports_images()),
        };
        let system = crate::prompt::render(
            crate::prompt::PromptTemplate::SystemAssembly,
            &crate::prompt::SystemPromptAssembly {
                base,
                language: is_subagent.then(crate::system_prompt::language_data),
                worktree_subagent: if is_subagent {
                    self.worktree
                        .as_ref()
                        .map(|w| crate::prompt::WorktreePromptData {
                            branch: w.branch.clone(),
                            path: w.path.display().to_string(),
                        })
                } else {
                    None
                },
                goal: self.goal.is_some(),
                ultracode: active_effort == ReasoningEffort::Ultracode,
                capabilities,
            },
        )
        .expect("system assembly template render");
        // The system prompt is the head of the cached prefix. `cache_control`
        // breakpoints are placed by `provider::anthropic_cache::apply_prompt_caching`
        // (single source of truth); the `cache` flag is advisory metadata, not
        // read by the wire mapper today — kept aligned with that intent.
        messages.push(LanguageModelRequestMessage {
            role: Role::System,
            content: vec![MessageContent::Text(system)],
            cache: true,
        });
        // Multi-level CLAUDE.md instructions ride as a per-request user
        // message in the fixed slot right after the system head — the same
        // contract as the `<collaboration_mode>` block below: never written
        // into `self.messages`, byte-stable while the files on disk are
        // unchanged, so the prefix cache treats it as part of the system
        // head. An empty load set omits the message entirely, keeping the
        // prefix byte-identical to a session with no instruction files.
        if self.instructions_enabled
            && let Some(text) = crate::claude_md::render_eager(&crate::claude_md::load(
                &self.cwd,
                &crate::settings::claude_md_load_context(),
            ))
        {
            messages.push(LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(text)],
                cache: true,
            });
        }
        // Inject the active mode's `developer_instructions` as a fixed-position
        // user-role message wrapped in a `<collaboration_mode>` tag — the
        // codex port's equivalent of the developer-role message. manox has no
        // Developer role, so it rides as User. Main-thread only: sub-agents
        // carry their own `agents/*.md` system body and run in `Default`.
        // Never written into `self.messages` (it is per-request, mode-stable),
        // so toggling mode does not rewrite history — the system head before it
        // stays byte-stable, and the block sits at a fixed slot so the prefix
        // cache stays warm across mode-stable turns.
        if self.depth == 0
            && let Some(instructions) = self.active_mode_settings().developer_instructions
        {
            messages.push(LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(format!(
                    "<collaboration_mode>\n{instructions}\n</collaboration_mode>"
                ))],
                cache: true,
            });
        }
        // Map canonical messages to the request, stripping the `agent` tool's
        // JSON envelope to just its `final` text. The full sub-conversation
        // stays in `self.messages` for persistence and UI rebuild, but the
        // parent model must only see the final reply — otherwise every sub-agent
        // tool call, tool result, and reasoning block leaks into the parent's
        // context, defeating the point of spawning an isolated sub-agent.
        //
        // When a compaction message sits in the history, the request is
        // assembled as `[retained recent user messages][compaction summary]
        // [everything after the compaction]` instead of the full transcript —
        // the older pre-compaction history is dropped (it lives on only inside
        // the summary). The retained tail keeps recent user prompts verbatim so
        // the active request + its tool results stay grounded; see `compact`.
        // Adjacent same-role runs that the assembly can produce
        // (retained-user → compaction-user → …, or a plan-mode continue
        // ending the turn on a user-role ToolResult followed by the user's
        // next message) are coalesced so Anthropic's wire, which rejects
        // consecutive same-role messages, accepts the request. A no-op for
        // the normal alternation, so the cached prefix is byte-stable across
        // turns; the coalesced run is itself stable once it's in history.
        let mapped: Vec<LanguageModelRequestMessage> = crate::compact::coalesce_same_role(
            match crate::compact::latest_compaction_ix(&self.messages, self.messages.len()) {
                Some(c_ix) => {
                    let mut rebuilt =
                        crate::compact::retained_user_messages_before(&self.messages, c_ix);
                    // The compaction message itself (mapped to its preamble-wrapped
                    // text form). Always included — it is the boundary marker.
                    let compaction_content: Vec<MessageContent> = self.messages[c_ix]
                        .content
                        .iter()
                        .map(model_facing_content)
                        .collect();
                    rebuilt.push(LanguageModelRequestMessage {
                        role: self.messages[c_ix].role,
                        content: compaction_content,
                        cache: false,
                    });
                    // Everything after the compaction verbatim (mapped), including
                    // the active tool-result turn.
                    rebuilt.extend(self.messages[c_ix + 1..].iter().map(|m| {
                        LanguageModelRequestMessage {
                            role: m.role,
                            content: m.content.iter().map(model_facing_content).collect(),
                            cache: false,
                        }
                    }));
                    rebuilt
                }
                None => self
                    .messages
                    .iter()
                    .map(|m| LanguageModelRequestMessage {
                        role: m.role,
                        content: m.content.iter().map(model_facing_content).collect(),
                        cache: false,
                    })
                    .collect(),
            },
        );
        // The `cache` flag marks the trailing two user/assistant messages as
        // cache-anchor candidates. It is advisory metadata today — the actual
        // `cache_control` breakpoints are placed by `apply_prompt_caching`
        // against messages[-2]/messages[-1] — but keeping the flag aligned with
        // that intent documents the contract for a future wire mapper that
        // reads it.
        let len = mapped.len();
        for (i, mut m) in mapped.into_iter().enumerate() {
            if i + 2 >= len {
                m.cache = true;
            }
            messages.push(m);
        }
        // A model that declares `supports_tools: false` (provider-config ground
        // truth, not model self-report) runs in pure-conversation mode: no tools
        // array. The wire mappers omit `tools` when the list is empty, so the
        // request stays a plain chat completion. Plan mode is gated by the same
        // flag — there is nothing to plan without tools.
        let supports_tools = active_model.as_ref().is_none_or(|m| m.supports_tools());
        let tools = if !supports_tools {
            Vec::new()
        } else if self.collaboration_mode == ModeKind::Plan {
            // In Plan mode, advertise the read-only tool set only — the model
            // submits a plan by emitting a `<proposed_plan>` block, not by
            // calling a submit tool. The `agent` tool is read-only
            // (`SpawnAgentTool::is_read_only`), so it is included, letting the
            // main thread delegate research to the `explore` sub-agent with
            // isolated context. Write tools and `bash` stay hidden;
            // `run_tool_inner` backstops any stray write call. Plan mode takes
            // precedence over `turn_tool_filter` — it is the stricter,
            // user-visible safety contract.
            self.tools.to_request_tools_read_only()
        } else {
            // A slash command's `allowed-tools` whitelist is an intentional
            // narrowing of this turn's tool set. Respect it; the model enters
            // Plan mode only via an explicit user action (`/plan`, the `+` menu,
            // shift-tab), never by a synthesized tool.
            match self.turn_tool_filter.as_deref() {
                Some(f) if !f.is_empty() => self.tools.to_request_tools_filtered(f),
                _ => self.tools.to_request_tools(),
            }
        };
        // Coalesce the fully assembled list so the fixed-position
        // `<collaboration_mode>` User message merges with a leading User turn
        // instead of producing adjacent same-role messages Anthropic rejects.
        // The mapped history was already coalesced above; this pass only
        // touches the instruction↔first-turn boundary (and is idempotent on
        // the rest). Cache flags survive — coalesce keeps the trailing
        // segment's flag, so the prefix and trailing cache anchors persist.
        let messages = crate::compact::coalesce_same_role(messages);
        LanguageModelRequest {
            messages,
            tools,
            reasoning_effort: Some(active_effort.resolve_for_wire(self.depth)),
            ..Default::default()
        }
    }
}

/// Strip the `agent` tool's JSON envelope from a ToolResult so only its `final`
/// text reaches the model. The canonical `Thread::messages` keep the full
/// envelope (for persistence and UI rebuild); this mapping is applied only when
/// building a request, so the sub-conversation never leaks into the parent's
/// context. Non-`agent` content passes through unchanged.
///
/// `Compaction` blocks are rewritten into a `Text` block wrapped with a preamble
/// so the model reads the summary as user-supplied context rather than a raw
/// opaque blob — the `Compaction` variant itself never reaches a provider wire
/// mapper.
pub(crate) fn model_facing_content(c: &MessageContent) -> MessageContent {
    match c {
        MessageContent::ToolResult(tr) if tr.tool_name.as_ref() == crate::tools::AGENT => {
            MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: tr.tool_use_id.clone(),
                tool_name: tr.tool_name.clone(),
                is_error: tr.is_error,
                content: crate::tools::agent::agent_final_text(&tr.content),
            })
        }
        MessageContent::Compaction(summary) => MessageContent::Text(
            crate::prompt::render(
                crate::prompt::PromptTemplate::WrapperCompactionPreamble,
                &crate::prompt::CompactionPreambleData {
                    summary: summary.clone(),
                },
            )
            .expect("compaction preamble render"),
        ),
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
    use crate::tools;
    match name {
        x if x == tools::READ || x == tools::WRITE || x == tools::LIST => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            format!("{name} {path}")
        }
        // edit_file's input is a single `patch` string whose first `[PATH#TAG]`
        // header names the target file. The path is everything before the last
        // `#`, so paths containing `#` survive.
        x if x == tools::EDIT => {
            let patch = input.get("patch").and_then(|v| v.as_str()).unwrap_or("");
            let path = patch
                .lines()
                .find_map(|l| {
                    let l = l.trim();
                    let inner = l.strip_prefix('[')?.strip_suffix(']')?;
                    Some(inner.rsplit_once('#')?.0.to_string())
                })
                .unwrap_or_default();
            format!("Edit {path}")
        }
        x if x == tools::BASH => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let single = cmd.lines().next().unwrap_or("").trim().to_string();
            if single.chars().count() > 80 {
                let t: String = single.chars().take(80).collect();
                format!("{t}…")
            } else {
                single
            }
        }
        x if x == tools::GREP => {
            let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("Grep {p}")
        }
        x if x == tools::GLOB => {
            let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("Glob {p}")
        }
        x if x == tools::AGENT => {
            let st = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let description = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let trimmed = if description.chars().count() > 60 {
                let t: String = description.chars().take(60).collect();
                format!("{t}…")
            } else {
                description.to_string()
            };
            if trimmed.is_empty() {
                st.to_string()
            } else {
                format!("{st} · {trimmed}")
            }
        }
        x if x == tools::ASK_USER_QUESTION => {
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
        // `web_fetch` and the read-side browser tools surface the target URL so
        // the activity line reads like a Claude Code Fetch/Read action.
        x if x == tools::WEB_FETCH => {
            let u = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            format!("Fetch {u}")
        }
        "WebExploreOpen" | "WebExploreNavigate" => {
            let u = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            format!("{name} {u}")
        }
        "WebExploreReadText" | "WebExploreReadDom" | "WebExploreScreenshot" => {
            // Tab-scoped reads surface the tab id when present; fall back to the
            // tool name so a tab-less historical entry still reads cleanly.
            let tab = input
                .get("tab_id")
                .and_then(|v| v.as_u64())
                .map(|t| format!("tab {t}"))
                .unwrap_or_else(|| name.to_string());
            format!("{name} {tab}")
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

    /// Serializes tests that touch the process-global `ThreadStore`
    /// (`init_for_test`/`global()`). Two such tests running in parallel
    /// stomp each other's `TEST_OVERRIDE` entity and crash gpui's entity
    /// map, so they must not overlap. Acquire at the top of each test that
    /// calls `init_for_test`.
    static THREAD_STORE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn edit_file_title_extracts_path_not_tag() {
        // The `[PATH#TAG]` header must surface the path, not the 4-hex tag.
        let input = json!({ "patch": "[src/main.rs#1A2B]\nSWAP 5.=5:\n+X" });
        assert_eq!(tool_title("Edit", &input), "Edit src/main.rs");
    }

    #[test]
    fn edit_file_title_path_with_hash_survives() {
        // A path containing `#` keeps everything before the LAST `#`.
        let input = json!({ "patch": "[a/b#issue.rs#FF0E]\nDEL 1" });
        assert_eq!(tool_title("Edit", &input), "Edit a/b#issue.rs");
    }

    #[test]
    fn edit_file_title_missing_header_is_empty() {
        let input = json!({ "patch": "SWAP 5.=5:\n+X" });
        assert_eq!(tool_title("Edit", &input), "Edit ");
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
            tool_title("Bash", &input),
            "gh pr create -R dspo/manox --title foo"
        );

        let long = "a".repeat(120);
        let input = json!({ "command": long.clone() });
        let out = tool_title("Bash", &input);
        assert_eq!(out.chars().count(), 81);
        assert!(out.ends_with('…'));

        let multi = "git status\ncargo build --release";
        let input = json!({ "command": multi });
        assert_eq!(tool_title("Bash", &input), "git status");

        let input = json!({});
        assert_eq!(tool_title("Bash", &input), "");
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
            tool_name: Arc::from(crate::tools::AGENT),
            is_error: false,
            content: envelope,
        });
        let stripped = model_facing_content(&tr);
        let MessageContent::ToolResult(out) = stripped else {
            panic!("expected ToolResult");
        };
        assert_eq!(out.content, "found 3 files");
        assert_eq!(out.tool_name.as_ref(), crate::tools::AGENT);
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
            tool_name: Arc::from(crate::tools::BASH),
            is_error: false,
            content: "command output".to_string(),
        });
        let MessageContent::ToolResult(out) = model_facing_content(&tr) else {
            panic!("expected ToolResult");
        };
        assert_eq!(out.content, "command output");
        assert_eq!(out.tool_name.as_ref(), crate::tools::BASH);
    }

    /// A legacy `agent` result (plain text, no envelope) falls back to the raw
    /// content rather than emptying it.
    #[test]
    fn model_facing_content_agent_legacy_falls_back() {
        let tr = MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: "tu_3".to_string(),
            tool_name: Arc::from(crate::tools::AGENT),
            is_error: false,
            content: "plain summary".to_string(),
        });
        let MessageContent::ToolResult(out) = model_facing_content(&tr) else {
            panic!("expected ToolResult");
        };
        assert_eq!(out.content, "plain summary");
    }

    /// A `Compaction` block never reaches a provider wire mapper as-is;
    /// `model_facing_content` rewrites it into a `Text` block carrying the
    /// summary prefixed with a preamble so the model treats it as user-supplied
    /// context. The original `Compaction` is untouched (canonical storage).
    #[test]
    fn model_facing_content_rewrites_compaction_to_text() {
        let block = MessageContent::Compaction("Goal: ship recaps.".to_string());
        let facing = model_facing_content(&block);
        let MessageContent::Text(text) = facing else {
            panic!("expected Text, got {facing:?}");
        };
        assert!(text.starts_with("The previous conversation was compacted."));
        assert!(text.contains("Goal: ship recaps."));
        // Original stays a Compaction for persistence / UI rebuild.
        assert!(matches!(block, MessageContent::Compaction(_)));
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
            name: Arc::from(crate::tools::SELF_INFO),
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

    #[test]
    fn cancel_emits_explicit_cancelled_stop_reason() {
        use crate::language_model::StopReason;
        use std::sync::Mutex;
        use tokio_util::sync::CancellationToken;

        use super::ThreadEvent;

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-cancel-reason", "/tmp", Vec::new()),
                None,
                cx,
            )
        });
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let captured = reasons.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&thread, move |_, event: &ThreadEvent, _| {
                if let ThreadEvent::Stop(reason) = event {
                    captured.lock().unwrap().push(*reason);
                }
            })
        });
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.turn_cancel = Some(CancellationToken::new());
                thread.cancel(cx);
            });
        });
        assert_eq!(reasons.lock().unwrap().as_slice(), &[StopReason::Cancelled]);
    }

    /// A leader `cancel` propagates to every worker member: their in-flight
    /// turns are part of the same coordinated effort, so stopping the leader
    /// must not leave members running. A worker's own `cancel` does NOT
    /// cascade to siblings (only depth-0 threads propagate).
    #[test]
    fn leader_cancel_propagates_to_team_members() {
        use crate::team::{Member, Team};

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();

        let (leader, member, team, member_token) = cx.update(|cx| {
            let leader = super::Thread::restore(
                crate::db::ThreadRecord::for_test("lead", "/tmp", Vec::new()),
                None,
                cx,
            );
            let mut member_rec = crate::db::ThreadRecord::for_test("plan", "/tmp", Vec::new());
            member_rec.depth = 1;
            let member = super::Thread::restore(member_rec, None, cx);
            let team = Team::new("squad".into(), leader.downgrade(), cx);
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member.clone()),
                    cx,
                )
            })
            .unwrap();
            leader.update(cx, |t, cx| t.set_team(team.clone(), cx));
            let token = tokio_util::sync::CancellationToken::new();
            let clone = token.clone();
            member.update(cx, |t, _| {
                t.turn_cancel = Some(clone);
            });
            (leader, member, team, token)
        });

        // Cancel the leader. Its depth-0 gate trips the propagation path, which
        // calls `cancel` on every member thread.
        cx.update(|cx| {
            leader.update(cx, |t, cx| t.cancel(cx));
        });
        cx.run_until_parked();

        assert!(member_token.is_cancelled(), "member turn token cancelled");
        assert!(
            cx.update(|cx| member.read(cx).turn_cancel.is_none()),
            "member turn_cancel taken"
        );
        // `_team` is held for the assertion lifetime; drop after.
        drop(team);
    }

    /// ToolResult to the message list BEFORE clearing `plan_mode`. The old
    /// code set `plan_mode = false` first; if `append_tool_result` then
    /// failed (entity gone, infra error), the thread would exit plan mode
    /// without recording the approval — inconsistent state for the next
    /// `build_completion_request`.
    /// While `run_plan_approval` is parked, the plan text must be queryable via
    /// `pending_plan_entries` so the workspace can restore its `pending_plan`
    /// after a background→foreground round-trip, and the entry must be gone
    /// once the approval resolves. Regression for the hang where a backgrounded
    /// plan approval lost its UI on reclaim.
    /// `build_completion_request` must fold `Auto` / `Ultracode` into concrete
    /// wire levels before the request reaches a provider, and append the
    /// Ultracode multi-agent grant to the system prompt. The raw enum never
    /// reaches the wire — see `ReasoningEffort::resolve_for_wire`.
    #[test]
    fn build_completion_request_resolves_auto_and_ultracode_effort() {
        use crate::language_model::{MessageContent, ReasoningEffort, Role};

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-effort-resolve", "/tmp", Vec::new()),
                None,
                cx,
            )
        });

        // Auto on the main thread (depth 0) → High.
        let req = cx.update(|cx| {
            thread.update(cx, |t, cx| {
                t.set_reasoning_effort(ReasoningEffort::Auto, cx)
            });
            thread.read(cx).build_completion_request()
        });
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::High));

        // Ultracode → XHigh on the wire, and the grant lands in the system
        // prompt (the behavioral half of the Claude Code ultracode semantic).
        let req = cx.update(|cx| {
            thread.update(cx, |t, cx| {
                t.set_reasoning_effort(ReasoningEffort::Ultracode, cx)
            });
            thread.read(cx).build_completion_request()
        });
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::XHigh));
        let sys_text = req
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    MessageContent::Text(s) => Some(s.as_str()),
                    _ => None,
                })
            })
            .expect("system message with text");
        assert!(
            sys_text.contains("Ultracode mode"),
            "ultracode grant must be in the system prompt, got: {sys_text}"
        );
    }

    /// The multi-level CLAUDE.md block rides as a per-request user message in
    /// the fixed slot right after the system head: present when instruction
    /// files exist under cwd, byte-stable across builds while the files are
    /// unchanged (prefix-cache contract), and omitted entirely when the
    /// thread disables instructions (the built-in explore carve-out) or when
    /// nothing loads.
    #[test]
    fn build_completion_request_injects_instructions_at_fixed_slot() {
        use crate::language_model::Role;

        let tmp = std::env::temp_dir().join(format!(
            "manox_thread_claude_md_{}_{}",
            std::process::id(),
            "slot"
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("CLAUDE.md"), "PROJECT RULE MARKER").unwrap();

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let cwd = tmp.to_string_lossy().to_string();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("claude-md-slot", &cwd, Vec::new()),
                None,
                cx,
            )
        });

        let req = cx.update(|cx| thread.read(cx).build_completion_request());
        assert_eq!(req.messages[0].role, Role::System, "system head first");
        assert_eq!(req.messages[1].role, Role::User, "instructions slot");
        let block = text_of_req(&req.messages[1]);
        assert!(block.contains("PROJECT RULE MARKER"), "{block}");
        assert!(block.contains("scope=\"project\""), "{block}");
        assert!(req.messages[1].cache, "instructions block is cacheable");

        // Byte-stable across builds while the files on disk are unchanged.
        let req2 = cx.update(|cx| thread.read(cx).build_completion_request());
        assert_eq!(block, text_of_req(&req2.messages[1]));

        // Disabled (built-in explore carve-out) → the slot vanishes.
        cx.update(|cx| {
            thread.update(cx, |t, _cx| t.set_instructions_enabled(false));
        });
        let req3 = cx.update(|cx| thread.read(cx).build_completion_request());
        assert!(
            req3.messages
                .iter()
                .all(|m| !text_of_req(m).contains("PROJECT RULE MARKER")),
            "disabled thread must not carry instructions"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A successful `read_file` appends newly-discovered nested CLAUDE.md
    /// files to its own tool result as a `<system-reminder>` — exactly once
    /// per directory (`lazy_instructions_injected` dedups), while error
    /// results, other tools, selector-suffixed paths, and the explore
    /// carve-out stay untouched.
    #[test]
    fn read_file_tool_result_injects_nested_instructions_once() {
        let tmp = std::env::temp_dir().join(format!(
            "manox_thread_lazy_{}_{}",
            std::process::id(),
            "inject"
        ));
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::create_dir_all(tmp.join("other")).unwrap();
        std::fs::write(tmp.join("sub/CLAUDE.md"), "NESTED RULE MARKER").unwrap();
        std::fs::write(tmp.join("sub/file.rs"), "fn main() {}").unwrap();
        std::fs::write(tmp.join("other/CLAUDE.md"), "OTHER DIR MARKER").unwrap();
        std::fs::write(tmp.join("other/f.rs"), "fn f() {}").unwrap();

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let cwd = tmp.to_string_lossy().to_string();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("lazy-inject", &cwd, Vec::new()),
                None,
                cx,
            )
        });
        let tool_use = |path: &str, name: &str| crate::language_model::LanguageModelToolUse {
            id: "t1".into(),
            name: name.into(),
            raw_input: String::new(),
            input: serde_json::json!({"path": path}),
            is_input_complete: true,
            thought_signature: None,
        };

        // Successful read of a file under `sub/`: reminder rides the result.
        let out = cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.maybe_augment_read_instructions(
                    &tool_use("sub/file.rs", "read_file"),
                    "FILE BODY".to_string(),
                    false,
                )
            })
        });
        assert!(out.starts_with("FILE BODY"), "{out}");
        assert!(out.contains("<system-reminder>"), "{out}");
        assert!(out.contains("NESTED RULE MARKER"), "{out}");

        // Second read in the same directory: no duplicate injection.
        let out = cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.maybe_augment_read_instructions(
                    &tool_use("sub/file.rs", "read_file"),
                    "FILE BODY".to_string(),
                    false,
                )
            })
        });
        assert_eq!(out, "FILE BODY");

        // Selector suffixes are stripped before discovery (`other/` fires).
        let out = cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.maybe_augment_read_instructions(
                    &tool_use("other/f.rs:raw", "read_file"),
                    "RAW BODY".to_string(),
                    false,
                )
            })
        });
        assert!(out.contains("OTHER DIR MARKER"), "{out}");

        // Error results and non-read_file tools are never augmented.
        let out = cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.maybe_augment_read_instructions(
                    &tool_use("sub/file.rs", "read_file"),
                    "ERR".to_string(),
                    true,
                )
            })
        });
        assert_eq!(out, "ERR");
        let out = cx.update(|cx| {
            thread.update(cx, |t, _cx| {
                t.maybe_augment_read_instructions(
                    &tool_use("sub/file.rs", "bash"),
                    "SHELL OUT".to_string(),
                    false,
                )
            })
        });
        assert_eq!(out, "SHELL OUT");

        // The explore carve-out (`instructions_enabled = false`) also gates
        // the lazy layer: a fresh thread with instructions disabled leaves
        // the result untouched and marks nothing injected.
        let thread2 = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("lazy-inject-off", &cwd, Vec::new()),
                None,
                cx,
            )
        });
        cx.update(|cx| {
            thread2.update(cx, |t, _cx| t.set_instructions_enabled(false));
        });
        let out = cx.update(|cx| {
            thread2.update(cx, |t, _cx| {
                t.maybe_augment_read_instructions(
                    &tool_use("sub/file.rs", "read_file"),
                    "FILE BODY".to_string(),
                    false,
                )
            })
        });
        assert_eq!(out, "FILE BODY");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn text_of_req(m: &crate::language_model::LanguageModelRequestMessage) -> String {
        m.content
            .iter()
            .filter_map(|c| match c {
                crate::language_model::MessageContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// A model declaring `supports_tools: false` runs in pure-conversation mode:
    /// `build_completion_request` omits the tools array (no registry tools) and
    /// the system prompt carries the ground-truth "no tools" line — so a
    /// toolless model cannot claim tool ability. Flipping `supports_tools`/
    /// `supports_images` to true restores the tool list and advertises image
    /// capability (thread 480b2469: a non-multimodal model claimed multimodal
    /// ability; this is the ground truth that stops it).
    #[test]
    fn build_completion_request_gates_tools_and_images_on_capability() {
        use crate::language_model::{MessageContent, Role};

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();

        // Toolless, imageless model.
        let no_tools: AnyLanguageModel = Arc::new(CapabilityMockModel {
            id: "test/no-tools".into(),
            supports_tools: false,
            supports_images: false,
        });
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-cap-no-tools", "/tmp", Vec::new()),
                Some(no_tools),
                cx,
            )
        });
        let req = cx.update(|cx| thread.read(cx).build_completion_request());
        assert!(
            req.tools.is_empty(),
            "supports_tools=false must omit all tools (incl. plan tools)"
        );
        let sys_text = req
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    MessageContent::Text(s) => Some(s.as_str()),
                    _ => None,
                })
            })
            .expect("system message");
        assert!(
            sys_text.contains("You have no tools"),
            "ground-truth no-tools line missing: {sys_text}"
        );
        assert!(
            sys_text.contains("cannot process images"),
            "ground-truth no-images line missing: {sys_text}"
        );

        // Capable model: tools advertised and image capability announced.
        let capable: AnyLanguageModel = Arc::new(CapabilityMockModel {
            id: "test/capable".into(),
            supports_tools: true,
            supports_images: true,
        });
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-cap-able", "/tmp", Vec::new()),
                Some(capable),
                cx,
            )
        });
        let req = cx.update(|cx| thread.read(cx).build_completion_request());
        assert!(
            !req.tools.is_empty(),
            "supports_tools=true must advertise the tool list"
        );
        let sys_text = req
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    MessageContent::Text(s) => Some(s.as_str()),
                    _ => None,
                })
            })
            .expect("system message");
        assert!(
            sys_text.contains("can process image"),
            "ground-truth image-capability line missing: {sys_text}"
        );
    }

    /// Regression guard: a fresh Default-mode main thread carries the mode's
    /// `developer_instructions` as a fixed-position `<collaboration_mode>` User
    /// message injected at request-build time (system-after, history-before).
    /// When the first persisted message is itself a User turn — the normal case —
    /// the instruction message is adjacent to it. Anthropic's wire rejects
    /// adjacent same-role messages, so the assembled list must be coalesced
    /// before it reaches a provider: the instruction folds into the leading User
    /// turn instead of standing as a second, adjacent User message. This asserts
    /// the wire-facing message list (System extracted to the top-level system
    /// field, as `provider::anthropic` does) alternates strictly User↔Assistant.
    #[test]
    fn build_completion_request_no_adjacent_same_role_after_mode_injection() {
        use crate::language_model::{MessageContent, Role};

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let capable: AnyLanguageModel = Arc::new(CapabilityMockModel {
            id: "test/capable".into(),
            supports_tools: true,
            supports_images: false,
        });
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-no-adjacent-role", "/tmp", Vec::new()),
                Some(capable),
                cx,
            )
        });
        // First turn: a lone User message — exactly the adjacency the
        // injection creates if left uncoalesced.
        cx.update(|cx| {
            thread.update(cx, |t, cx| t.insert_user_message("hello".into(), cx));
        });
        let req = cx.update(|cx| thread.read(cx).build_completion_request());

        // The wire-facing list drops System-role messages (Anthropic hoists
        // them to the top-level `system` field). Assert what remains
        // alternates strictly — no two consecutive messages share a role.
        let wire: Vec<Role> = req
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| m.role)
            .collect();
        assert!(
            !wire.is_empty(),
            "expected at least the first user turn on the wire"
        );
        for pair in wire.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "adjacent same-role messages on the wire: {wire:?}"
            );
        }
        // The mode instruction must ride inside the first User message (coalesced),
        // not as a separate adjacent User — so the first wire User carries both the
        // `<collaboration_mode>` block and "hello" (coalesce appends content as
        // separate blocks, so read every text block, not just the first).
        let first_user_text: String = req
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|c| match c {
                        MessageContent::Text(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .expect("at least one user message");
        assert!(
            first_user_text.contains("<collaboration_mode>"),
            "mode instruction missing from the first user message: {first_user_text}"
        );
        assert!(
            first_user_text.contains("hello"),
            "first user turn missing from the coalesced message: {first_user_text}"
        );
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
            name: Arc::from(crate::tools::ASK_USER_QUESTION),
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
        auto_compact_window: Option<u64>,
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
        fn auto_compact_window(&self) -> Option<u64> {
            self.auto_compact_window
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

    /// A mock model whose only configurable surface is the two capability flags,
    /// so `build_completion_request`'s tool-gating and system-prompt ground-truth
    /// can be exercised without a wire round-trip. `supports_tools == false`
    /// must yield an empty tools list and a "no tools" capability line;
    /// `supports_images` must drive the image-capability line.
    struct CapabilityMockModel {
        id: String,
        supports_tools: bool,
        supports_images: bool,
    }

    impl crate::language_model::LanguageModel for CapabilityMockModel {
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
        fn supports_tools(&self) -> bool {
            self.supports_tools
        }
        fn supports_images(&self) -> bool {
            self.supports_images
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
            Box::pin(async move {
                use futures::StreamExt as _;
                Ok(futures::stream::empty().boxed())
            })
        }
    }

    /// A `ReplayMockModel` that also counts `stream_completion` invocations via
    /// a shared atomic, so a test can assert the outer turn loop re-requested
    /// the model — the proof that a parse error or `MaxTokens` truncation was
    /// fed back to the model rather than dropped on a bare `break`.
    struct CountingReplayMockModel {
        id: String,
        events: Arc<Vec<LanguageModelCompletionEvent>>,
        calls: Arc<std::sync::atomic::AtomicU64>,
    }

    impl crate::language_model::LanguageModel for CountingReplayMockModel {
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
            let calls = self.calls.clone();
            Box::pin(async move {
                use futures::StreamExt as _;
                use std::sync::atomic::Ordering;
                let _ = calls.fetch_add(1, Ordering::SeqCst);
                let events: Vec<_> = events.iter().cloned().map(Ok).collect();
                Ok(futures::stream::iter(events).boxed())
            })
        }
    }

    /// Regression for the parse-error bare-break freeze: a stream that emits a
    /// `ToolUseJsonParseError` (malformed tool-call JSON) and then closes
    /// enqueues no `pending_tool_uses`, so a naive turn loop would bare-break
    /// on the empty collection and the error tool_result would sit in history
    /// never fed back to the model — the thread froze on an orphan result
    /// (thread 76aef71a). The recovery branch must re-request the model (the
    /// error result is already in history) until `MAX_RECOVERY_ATTEMPTS`
    /// aborts. This asserts the model was actually called again and that the
    /// error result landed in history.
    #[test]
    fn run_turn_feeds_parse_error_back_to_model() {
        use crate::language_model::LanguageModelToolResult;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        let db = Arc::new(
            crate::db::ThreadsDatabase::open(std::path::Path::new(":memory:"))
                .expect("open mem db"),
        );
        cx.update(|cx| {
            crate::thread_store::init_for_test(db, cx);
        });

        let calls = Arc::new(AtomicU64::new(0));
        let model: AnyLanguageModel = Arc::new(CountingReplayMockModel {
            id: "test/parse-error".into(),
            events: Arc::new(vec![
                LanguageModelCompletionEvent::Text("let me read the file".into()),
                LanguageModelCompletionEvent::ToolUseJsonParseError {
                    id: "tu_parse_1".into(),
                    tool_name: Arc::from(crate::tools::READ),
                    raw_input: "{bad".into(),
                    json_parse_error: "expected `}`".into(),
                },
            ]),
            calls: calls.clone(),
        });

        let thread_id = "reg-parse-error-feedback";
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                thread_id,
                "/tmp",
                vec![Message::user("read foo".into())],
            );
            // Pin the title and run as a sub-agent (depth 1) so
            // `maybe_generate_title` bails on its `depth != 0` guard — an
            // appended error tool_result must not trip a topic-shift re-eval
            // and pollute the call counter.
            rec.title = Some("regression".into());
            let thread = super::Thread::restore(rec, Some(model), cx);
            thread.update(cx, |t, _| t.depth = 1);
            thread
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
        cx.run_until_parked();

        // 1 initial call + MAX_RECOVERY_ATTEMPTS re-requests: the parse error
        // must be fed back to the model instead of ending the turn on a bare
        // break with an orphaned error result. The final attempt hits the cap
        // and aborts.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1 + super::MAX_RECOVERY_ATTEMPTS as u64,
            "parse error must be fed back to the model, not dropped on a bare break"
        );

        // The error tool_result must be in history so the re-request carries it.
        cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                let has_error_result = t.messages.iter().any(|m| {
                    m.content.iter().any(|c| match c {
                        MessageContent::ToolResult(LanguageModelToolResult {
                            is_error: true,
                            content,
                            ..
                        }) => content.contains("Tool input JSON parse failed"),
                        _ => false,
                    })
                });
                assert!(
                    has_error_result,
                    "parse-error tool_result must be in history to feed back"
                );
            });
        });
    }

    /// Regression for the silent-truncation freeze: a stream that stops with
    /// `MaxTokens` and no actionable tool_use used to end the turn dead on the
    /// truncated assistant message. The recovery branch must re-request the
    /// model, appending a "redo compactly" directive (append-only, so the
    /// prefix cache stays intact and the truncated assistant messages are
    /// retained, not dropped) until `MAX_RECOVERY_ATTEMPTS` aborts. This
    /// asserts the model was retried, the directive was appended, and that
    /// every round's assistant message survives (proving the append-only
    /// contract — a drop-and-rewrite variant would leave fewer).
    #[test]
    fn run_turn_recovers_max_tokens_via_append_only_directive() {
        use crate::language_model::{MessageContent, Role, StopReason};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        let db = Arc::new(
            crate::db::ThreadsDatabase::open(std::path::Path::new(":memory:"))
                .expect("open mem db"),
        );
        cx.update(|cx| {
            crate::thread_store::init_for_test(db, cx);
        });

        let calls = Arc::new(AtomicU64::new(0));
        let model: AnyLanguageModel = Arc::new(CountingReplayMockModel {
            id: "test/max-tokens".into(),
            events: Arc::new(vec![
                LanguageModelCompletionEvent::Text("truncated mid-thought".into()),
                LanguageModelCompletionEvent::Stop(StopReason::MaxTokens),
            ]),
            calls: calls.clone(),
        });

        let thread_id = "reg-max-tokens-retry";
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                thread_id,
                "/tmp",
                vec![Message::user("write a long essay".into())],
            );
            // Pin the title and run as a sub-agent (depth 1) so
            // `maybe_generate_title` bails on its `depth != 0` guard — the
            // append-only max-tokens directive is a user-role text message that
            // increments the user-message count, which would otherwise trip a
            // topic-shift re-eval and spawn an extra model stream (polluting the
            // call counter). `for_test` leaves `max_turns` `None`, so the
            // sub-agent turn cap does not fire.
            rec.title = Some("regression".into());
            let thread = super::Thread::restore(rec, Some(model), cx);
            thread.update(cx, |t, _| t.depth = 1);
            thread
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
        cx.run_until_parked();

        // 1 initial call + MAX_RECOVERY_ATTEMPTS re-requests; the final attempt
        // hits the cap and aborts rather than silently ending on the first
        // truncation.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1 + super::MAX_RECOVERY_ATTEMPTS as u64,
            "MaxTokens truncation must be retried, not silently end the turn"
        );

        // The recovery is append-only: every round's truncated assistant message
        // is retained (one per round), never dropped. A drop-and-rewrite variant
        // would leave only the final round's assistant message.
        let (assistant_count, directive_count) = cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                let assistant_count = t
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::Assistant)
                    .count();
                let directive_count =
                    t.messages
                        .iter()
                        .filter(|m| m.role == Role::User)
                        .fold(0, |acc, m| {
                            let has_directive = m.content.iter().any(|c| match c {
                                MessageContent::Text(t) => {
                                    t.contains("cut off by the max_output_tokens budget")
                                }
                                _ => false,
                            });
                            if has_directive { acc + 1 } else { acc }
                        });
                (assistant_count, directive_count)
            })
        });
        assert_eq!(
            assistant_count,
            (1 + super::MAX_RECOVERY_ATTEMPTS) as usize,
            "append-only recovery must retain every round's assistant message, not drop them"
        );
        // The directive is appended on every retry that continues (not on the
        // final abort), so MAX_RECOVERY_ATTEMPTS directives — one fewer than the
        // assistant-message count, since the abort round keeps its assistant
        // message but appends no directive.
        assert_eq!(
            directive_count,
            super::MAX_RECOVERY_ATTEMPTS as usize,
            "a max_output_tokens directive must be appended on each continued retry"
        );
    }

    /// A model whose `stream_completion` always fails the handshake. Exercises
    /// the recovery nudge for handshake-level failures (network drop, auth
    /// error): without it the turn ended dead on the first error and the final
    /// summary round-trip was lost (thread 0b80401d).
    struct HandshakeErrorMockModel {
        id: Arc<str>,
        calls: Arc<std::sync::atomic::AtomicU64>,
    }

    impl crate::language_model::LanguageModel for HandshakeErrorMockModel {
        fn id(&self) -> String {
            self.id.to_string()
        }
        fn name(&self) -> String {
            self.id.to_string()
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
            let calls = self.calls.clone();
            Box::pin(async move {
                use std::sync::atomic::Ordering;
                let _ = calls.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!(
                    "synthetic handshake failure: connection reset"
                ))
            })
        }
    }

    /// A model whose stream emits one Text chunk then an `Err` mid-stream.
    /// Exercises the recovery nudge for mid-stream failures (SSE truncation,
    /// malformed event): the assistant message is half-finished and, with no
    /// tool_use produced, the empty-tool-use branch must nudge the model to
    /// retry or wrap up instead of ending the turn dead (thread 0b80401d).
    struct MidStreamErrorMockModel {
        id: Arc<str>,
        calls: Arc<std::sync::atomic::AtomicU64>,
    }

    impl crate::language_model::LanguageModel for MidStreamErrorMockModel {
        fn id(&self) -> String {
            self.id.to_string()
        }
        fn name(&self) -> String {
            self.id.to_string()
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
            let calls = self.calls.clone();
            Box::pin(async move {
                use futures::StreamExt as _;
                use std::sync::atomic::Ordering;
                let _ = calls.fetch_add(1, Ordering::SeqCst);
                let events: Vec<anyhow::Result<LanguageModelCompletionEvent>> = vec![
                    Ok(LanguageModelCompletionEvent::Text("partial work".into())),
                    Err(anyhow::anyhow!(
                        "synthetic mid-stream failure: SSE truncated"
                    )),
                ];
                Ok(futures::stream::iter(events).boxed())
            })
        }
    }

    /// Recovery for a handshake-level `stream_completion` failure: each error
    /// injects a user nudge (append-only, prefix cache stays intact) and the
    /// loop re-requests until `MAX_RECOVERY_ATTEMPTS` aborts — instead of ending
    /// the turn dead on the first failure.
    #[test]
    fn run_turn_recovers_handshake_stream_error() {
        use crate::language_model::{MessageContent, Role};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        let db = Arc::new(
            crate::db::ThreadsDatabase::open(std::path::Path::new(":memory:"))
                .expect("open mem db"),
        );
        cx.update(|cx| {
            crate::thread_store::init_for_test(db, cx);
        });

        let calls = Arc::new(AtomicU64::new(0));
        let model: AnyLanguageModel = Arc::new(HandshakeErrorMockModel {
            id: "test/handshake-err".into(),
            calls: calls.clone(),
        });

        let thread_id = "reg-handshake-stream-error";
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                thread_id,
                "/tmp",
                vec![Message::user("summarize the work and deliver".into())],
            );
            // Pin the title and run as a sub-agent (depth 1) so
            // `maybe_generate_title` bails on its `depth != 0` guard — the
            // appended recovery nudge is a user-role text message that
            // increments the user-message count, which would otherwise trip a
            // topic-shift re-eval and pollute the call counter.
            rec.title = Some("regression".into());
            let thread = super::Thread::restore(rec, Some(model), cx);
            thread.update(cx, |t, _| t.depth = 1);
            thread
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
        cx.run_until_parked();

        // 1 initial call + MAX_RECOVERY_ATTEMPTS re-requests: each handshake
        // failure injects a recovery nudge instead of ending the turn dead.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1 + super::MAX_RECOVERY_ATTEMPTS as u64,
            "handshake stream error must be recovered via nudge, not end the turn on the first failure"
        );

        // A recovery nudge must be appended on each continued retry (one fewer
        // than the call count — the final abort round appends nothing).
        let nudge_count = cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                t.messages
                    .iter()
                    .filter(|m| m.role == Role::User)
                    .fold(0, |acc, m| {
                        let has_nudge = m.content.iter().any(|c| match c {
                            MessageContent::Text(t) => t.contains("previous model request failed"),
                            _ => false,
                        });
                        if has_nudge { acc + 1 } else { acc }
                    })
            })
        });
        assert_eq!(
            nudge_count,
            super::MAX_RECOVERY_ATTEMPTS as usize,
            "a recovery nudge must be appended on each continued handshake-error retry"
        );
    }

    /// Recovery for a mid-stream `LanguageModelCompletionEvent::Err`: the
    /// assistant message is half-finished (one Text chunk, no tool_use), so the
    /// empty-tool-use branch must pick up the stream-error flag and nudge the
    /// model to retry or wrap up — instead of ending the turn dead on the
    /// partial message (thread 0b80401d).
    #[test]
    fn run_turn_recovers_mid_stream_error() {
        use crate::language_model::{MessageContent, Role};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        let db = Arc::new(
            crate::db::ThreadsDatabase::open(std::path::Path::new(":memory:"))
                .expect("open mem db"),
        );
        cx.update(|cx| {
            crate::thread_store::init_for_test(db, cx);
        });

        let calls = Arc::new(AtomicU64::new(0));
        let model: AnyLanguageModel = Arc::new(MidStreamErrorMockModel {
            id: "test/mid-stream-err".into(),
            calls: calls.clone(),
        });

        let thread_id = "reg-mid-stream-error";
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                thread_id,
                "/tmp",
                vec![Message::user("summarize the work and deliver".into())],
            );
            rec.title = Some("regression".into());
            let thread = super::Thread::restore(rec, Some(model), cx);
            thread.update(cx, |t, _| t.depth = 1);
            thread
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
        cx.run_until_parked();

        // 1 initial call + MAX_RECOVERY_ATTEMPTS re-requests: each mid-stream
        // failure injects a recovery nudge instead of ending the turn dead on
        // the half-finished assistant message.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1 + super::MAX_RECOVERY_ATTEMPTS as u64,
            "mid-stream error must be recovered via nudge, not end the turn on the half-finished message"
        );

        // Every round's partial assistant message survives (append-only), one
        // per call — proving the half-finished messages are retained, not
        // dropped-and-rewritten.
        let (assistant_count, nudge_count) = cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                let assistant_count = t
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::Assistant)
                    .count();
                let nudge_count =
                    t.messages
                        .iter()
                        .filter(|m| m.role == Role::User)
                        .fold(0, |acc, m| {
                            let has_nudge = m.content.iter().any(|c| match c {
                                MessageContent::Text(t) => {
                                    t.contains("previous model request failed")
                                }
                                _ => false,
                            });
                            if has_nudge { acc + 1 } else { acc }
                        });
                (assistant_count, nudge_count)
            })
        });
        assert_eq!(
            assistant_count,
            (1 + super::MAX_RECOVERY_ATTEMPTS) as usize,
            "append-only recovery must retain every round's half-finished assistant message"
        );
        assert_eq!(
            nudge_count,
            super::MAX_RECOVERY_ATTEMPTS as usize,
            "a recovery nudge must be appended on each continued mid-stream-error retry"
        );
    }

    /// A model playing a scripted sequence of per-call outcomes, for overflow
    /// recovery tests: `Overflow` fails the request mid-stream with a
    /// classified `ContextOverflow`; `Text` completes with visible output.
    struct OverflowScriptMockModel {
        id: Arc<str>,
        calls: Arc<std::sync::atomic::AtomicU64>,
        script: Arc<Vec<OverflowCall>>,
    }

    enum OverflowCall {
        Overflow,
        Text(&'static str),
    }

    impl crate::language_model::LanguageModel for OverflowScriptMockModel {
        fn id(&self) -> String {
            self.id.to_string()
        }
        fn name(&self) -> String {
            self.id.to_string()
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
            let calls = self.calls.clone();
            let script = self.script.clone();
            Box::pin(async move {
                use futures::StreamExt as _;
                use std::sync::atomic::Ordering;
                let n = calls.fetch_add(1, Ordering::SeqCst) as usize;
                let call = script.get(n).or(script.last()).expect("non-empty script");
                let events: Vec<anyhow::Result<LanguageModelCompletionEvent>> = match call {
                    OverflowCall::Overflow => vec![Err(anyhow::Error::new(
                        crate::provider::overflow::ContextOverflow(
                            "Anthropic API returned 400 Bad Request: prompt is too long".into(),
                        ),
                    ))],
                    OverflowCall::Text(t) => vec![
                        Ok(LanguageModelCompletionEvent::Text((*t).into())),
                        Ok(LanguageModelCompletionEvent::Stop(
                            crate::language_model::StopReason::EndTurn,
                        )),
                    ],
                };
                Ok(futures::stream::iter(events).boxed())
            })
        }
    }

    /// Drives one scripted thread to completion and returns
    /// `(calls, message snapshot)`. `depth` exercises the sub-agent path.
    #[cfg(test)]
    fn run_scripted_overflow_thread(
        script: Vec<OverflowCall>,
        depth: u32,
    ) -> (u64, Vec<crate::message::Message>) {
        use crate::message::Message;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        let db = Arc::new(
            crate::db::ThreadsDatabase::open(std::path::Path::new(":memory:"))
                .expect("open mem db"),
        );
        cx.update(|cx| {
            crate::thread_store::init_for_test(db, cx);
        });

        let calls = Arc::new(AtomicU64::new(0));
        let model: AnyLanguageModel = Arc::new(OverflowScriptMockModel {
            id: "test/overflow-script".into(),
            calls: calls.clone(),
            script: Arc::new(script),
        });

        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                "reg-overflow-recovery",
                "/tmp",
                vec![Message::user("summarize the work and deliver".into())],
            );
            rec.title = Some("overflow recovery".into());
            let thread = super::Thread::restore(rec, Some(model), cx);
            thread.update(cx, |t, _| t.depth = depth);
            thread
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
        cx.run_until_parked();

        let messages = cx.update(|cx| thread.read_with(cx, |t, _| t.messages.clone()));
        let call_count = calls.load(std::sync::atomic::Ordering::SeqCst);
        (call_count, messages)
    }

    #[cfg(test)]
    fn count_nudges(messages: &[crate::message::Message]) -> usize {
        messages
            .iter()
            .filter(|m| m.role == crate::language_model::Role::User)
            .filter(|m| {
                m.content.iter().any(|c| match c {
                    crate::language_model::MessageContent::Text(t) => {
                        t.contains("previous model request failed")
                    }
                    _ => false,
                })
            })
            .count()
    }

    #[cfg(test)]
    fn count_compactions(messages: &[crate::message::Message]) -> usize {
        messages
            .iter()
            .filter(|m| {
                m.content
                    .iter()
                    .any(|c| matches!(c, crate::language_model::MessageContent::Compaction(_)))
            })
            .count()
    }

    /// A context-overflow stream error must NOT produce a recovery nudge (the
    /// identical request can never succeed); instead the turn compacts once
    /// and retries — and the retry then succeeds.
    #[test]
    fn run_turn_recovers_context_overflow_via_single_compaction() {
        let (calls, messages) = run_scripted_overflow_thread(
            vec![
                OverflowCall::Overflow,
                OverflowCall::Text("compacted summary"),
                OverflowCall::Text("final answer"),
            ],
            0,
        );
        assert_eq!(calls, 3, "request + compaction side call + retried request");
        assert_eq!(count_nudges(&messages), 0, "overflow never injects a nudge");
        assert_eq!(
            count_compactions(&messages),
            1,
            "exactly one compaction ran"
        );
        let final_text: String = messages
            .iter()
            .filter(|m| m.role == crate::language_model::Role::Assistant)
            .filter_map(|m| {
                m.content.iter().find_map(|c| match c {
                    crate::language_model::MessageContent::Text(t) => Some(t.clone()),
                    _ => None,
                })
            })
            .collect();
        assert!(
            final_text.contains("final answer"),
            "turn completes: {final_text}"
        );
    }

    /// The recovery budget is one: when the post-compaction retry overflows
    /// again, the turn aborts instead of looping compactions.
    #[test]
    fn run_turn_aborts_on_repeated_context_overflow() {
        let (calls, messages) = run_scripted_overflow_thread(
            vec![
                OverflowCall::Overflow,
                OverflowCall::Text("compacted summary"),
                OverflowCall::Overflow,
            ],
            0,
        );
        assert_eq!(
            calls, 3,
            "request + compaction + failed retry; no fourth call"
        );
        assert_eq!(count_compactions(&messages), 1, "no second compaction");
        assert_eq!(count_nudges(&messages), 0, "overflow never injects a nudge");
    }

    /// Sub-agents get the same overflow lifeline: depth does not disable the
    /// compact-and-retry rescue.
    #[test]
    fn run_turn_recovers_context_overflow_in_subagent() {
        let (calls, messages) = run_scripted_overflow_thread(
            vec![
                OverflowCall::Overflow,
                OverflowCall::Text("compacted summary"),
                OverflowCall::Text("final answer"),
            ],
            1,
        );
        assert_eq!(calls, 3);
        assert_eq!(count_compactions(&messages), 1);
    }

    /// `run_enter_plan_mode` flips `plan_mode` on, emits a success ToolCall,
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

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
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
            auto_compact_window: None,
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

    /// Regression for the stuck-sidebar-indicator bug: a turn whose stream
    /// ends WITHOUT a `MessageStop` (provider hiccup, non-SSE compatibility
    /// response) emits no terminal `Stop`/`Error` event, so the workspace —
    /// which clears the running indicator on those events — never learns the
    /// turn ended and the sidebar shimmer stays lit on the row forever. The
    /// `run_turn` task tail clears the indicator unconditionally as a backstop.
    #[test]
    fn run_turn_clears_running_indicator_without_stop_event() {
        use std::sync::Arc;

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        let db = Arc::new(
            crate::db::ThreadsDatabase::open(std::path::Path::new(":memory:"))
                .expect("open mem db"),
        );
        cx.update(|cx| {
            crate::thread_store::init_for_test(db, cx);
        });

        // Stream emits text and then closes with NO `Stop` — the bug scenario.
        let model: AnyLanguageModel = Arc::new(ReplayMockModel {
            id: "test/replay".into(),
            events: Arc::new(vec![LanguageModelCompletionEvent::Text(
                "hello world".into(),
            )]),
            auto_compact_window: None,
        });

        let thread_id = "reg-running-no-stop";
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                thread_id,
                "/tmp",
                vec![Message::user("ping".into())],
            );
            // Pin the title so `maybe_generate_title` short-circuits and does
            // not spawn a title stream task that outlives the test.
            rec.title = Some("regression".into());
            super::Thread::restore(rec, Some(model), cx)
        });

        // Simulate the workspace lighting the indicator on `TurnStarted` —
        // the state the bug leaves stuck when no terminal event follows.
        let store = crate::thread_store::global();
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_running(thread_id, cx));
        });
        cx.run_until_parked();
        assert!(
            cx.update(|cx| store.read_with(cx, |s, _| s.is_running(thread_id))),
            "indicator should be lit while the turn is in flight"
        );

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
        // The task-tail backstop runs in the same synchronous span as
        // `running_turn = None`; drain once more to be certain it has applied.
        cx.run_until_parked();

        assert!(
            !cx.update(|cx| store.read_with(cx, |s, _| s.is_running(thread_id))),
            "indicator must be cleared by the run_turn task-tail backstop when \
             the stream ends without a Stop event"
        );
    }

    /// `run_enter_plan_mode` flips `plan_mode` on, emits a success ToolCall,
    /// and appends a non-error ToolResult steering the model toward read-only
    /// research. No approval handshake — the turn loop continues and the next
    /// `build_completion_request` advertises the plan-mode tool set.
    /// `build_completion_request` advertises `enter_plan_mode` only on the main
    /// thread (depth 0) while not in plan mode, and `exit_plan_mode` only while
    /// in plan mode. The two are mutually exclusive by state, mirroring the
    /// synthesized-tool contract.
    /// Plan on/off must not rewrite the system head — the plan directive is a
    /// user message, so the provider prefix cache keeps its system+tools head
    /// intact across the switch. Before this fix, `set_plan_mode(true)`
    /// appended `PLAN_MODE_ADDENDUM` to `system` and `set_plan_mode(false)`
    /// removed it, so every plan enter/exit round-trip broke the cached system
    /// prefix (thread 0b80401d: the plan-exit round lost its 34万 cache_read).
    /// Regression guard: plan-mode continue ends the turn on a user-role
    /// ToolResult; the user's next message is also user-role. The two adjacent
    /// user messages must be coalesced before the request leaves
    /// `build_completion_request`, or Anthropic's wire rejects the turn with a
    /// 400. `coalesce_same_role` wraps the whole mapping — not just the
    /// compaction branch — so this adjacent-user run is normalized even when no
    /// compaction is in play.
    /// `auto_compaction_target` honors a model's `CLAUDE_CODE_AUTO_COMPACT_WINDOW`
    /// override: at 80% of the window it fires; below, it does not. The default
    /// (`None`) path falls back to `max_token_count` + the settings threshold.
    #[test]
    fn auto_compaction_target_uses_env_window_at_eighty_pct() {
        use crate::language_model::TokenUsage;

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let window = 202_745u64;
        let threshold_80 = ((window as f64) * 0.8).ceil() as u64; // 162_196
        // Mock model carrying the env-var window override.
        let model: AnyLanguageModel = Arc::new(ReplayMockModel {
            id: "test/auto-compact-window".into(),
            events: Arc::new(vec![]),
            auto_compact_window: Some(window),
        });
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                "reg-auto-compact-window",
                "/tmp",
                vec![Message::user("long session".into())],
            );
            rec.title = Some("reg".into());
            super::Thread::restore(rec, Some(model), cx)
        });

        // Below 80% → no compaction target.
        cx.update(|cx| {
            thread.update(cx, |t, _| {
                let uid = t.messages.last().unwrap().id.clone();
                t.token_meter.accumulate(TokenUsage {
                    input_tokens: threshold_80 - 1,
                    ..Default::default()
                });
                t.token_meter.finalize_request(Some(&uid));
            });
        });
        assert_eq!(
            cx.update(|cx| thread.read_with(cx, |t, _| t.auto_compaction_target())),
            None,
            "below 80% of the env window must not trigger"
        );

        // At 80% → compaction target (appends at the end — the trailing user
        // message already has reported usage).
        cx.update(|cx| {
            thread.update(cx, |t, _| {
                let uid = t.messages.last().unwrap().id.clone();
                t.token_meter.accumulate(TokenUsage {
                    input_tokens: threshold_80,
                    ..Default::default()
                });
                t.token_meter.finalize_request(Some(&uid));
            });
        });
        assert_eq!(
            cx.update(|cx| thread.read_with(cx, |t, _| t.auto_compaction_target())),
            Some(1),
            "at 80% of the env window the compaction target must fire"
        );
    }

    /// The `None` fallback path: a model without `auto_compact_window` uses
    /// `max_token_count` (4096 on the mock) + `settings.auto_compact.threshold`.
    /// Since 4096 < MIN_COMPACTION_CONTEXT_WINDOW (80_000), compaction is
    /// disabled — the `None` branch is taken, not the `Some` branch.
    #[test]
    fn auto_compaction_target_none_falls_back_to_max_token_count() {
        use crate::language_model::TokenUsage;

        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        // Mock model without the env-var override.
        let model: AnyLanguageModel = Arc::new(ReplayMockModel {
            id: "test/no-compact-window".into(),
            events: Arc::new(vec![]),
            auto_compact_window: None,
        });
        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                "reg-no-compact-window",
                "/tmp",
                vec![Message::user("session".into())],
            );
            rec.title = Some("reg".into());
            super::Thread::restore(rec, Some(model), cx)
        });

        // Even with high usage, compaction is disabled because the mock's
        // max_token_count (4096) is below MIN_COMPACTION_CONTEXT_WINDOW.
        cx.update(|cx| {
            thread.update(cx, |t, _| {
                let uid = t.messages.last().unwrap().id.clone();
                t.token_meter.accumulate(TokenUsage {
                    input_tokens: 500_000,
                    ..Default::default()
                });
                t.token_meter.finalize_request(Some(&uid));
            });
        });
        assert_eq!(
            cx.update(|cx| thread.read_with(cx, |t, _| t.auto_compaction_target())),
            None,
            "None auto_compact_window must fall back to max_token_count (below MIN), not use 0.8 threshold"
        );
    }

    /// `enqueue_steer` returns the queued message's id so the UI can pair a
    /// parked card to the drained message; `pending_steer_ids` exposes every
    /// still-queued id so the UI's terminal-Stop dead-letter check can spot
    /// stranded steers; `cancel_pending_steer` drops one by id so removing a
    /// SteerPending card cancels the injection (no invisible steer).
    #[test]
    fn enqueue_steer_ids_and_cancel_pending_steer() {
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-steer-ids", "/tmp", Vec::new()),
                None,
                cx,
            )
        });
        let id = cx.update(|cx| {
            thread.update(cx, |t, cx| {
                t.enqueue_steer(vec![MessageContent::Text("note".into())], None, cx)
            })
        });
        assert!(
            cx.update(|cx| thread.read_with(cx, |t, _| t.pending_steer_ids()))
                .contains(&id),
            "pending_steer_ids must report the just-enqueued steer"
        );
        let removed = cx.update(|cx| thread.update(cx, |t, _| t.cancel_pending_steer(&id)));
        assert!(
            removed,
            "cancel_pending_steer must report it removed the message"
        );
        assert!(
            !cx.update(|cx| thread.read_with(cx, |t, _| t.pending_steer_ids()))
                .contains(&id),
            "cancel_pending_steer must pull the message out of the steer queue"
        );
        // Cancelling an unknown id is a no-op, not a panic.
        assert!(
            !cx.update(|cx| thread.update(cx, |t, _| t.cancel_pending_steer("nope"))),
            "cancel_pending_steer on a missing id must return false"
        );
    }

    /// `drain_pending_steer` must move the message onto `messages` AND tag it
    /// `ui.steered` at drain time (not enqueue) — the marker is the persisted
    /// receipt that the running turn actually absorbed the steer, so a stranded
    /// enqueue never gets a badge.
    #[test]
    fn drain_pending_steer_tags_drained_message_steered() {
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        let thread = cx.update(|cx| {
            super::Thread::restore(
                crate::db::ThreadRecord::for_test("reg-steer-drain", "/tmp", Vec::new()),
                None,
                cx,
            )
        });
        let id = cx.update(|cx| {
            thread.update(cx, |t, cx| {
                t.enqueue_steer(vec![MessageContent::Text("note".into())], None, cx)
            })
        });
        let drained = cx.update(|cx| thread.update(cx, |t, cx| t.drain_pending_steer(cx)));
        assert!(
            drained,
            "drain_pending_steer must report it drained a message"
        );
        let steered = cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                t.messages
                    .iter()
                    .find(|m| m.id == id)
                    .and_then(|m| m.ui.as_ref())
                    .and_then(|ui| ui.steered)
            })
        });
        assert_eq!(
            steered,
            Some(true),
            "drained steer must be tagged steered (persisted receipt of injection)"
        );
    }

    #[test]
    fn announces_tool_intent_without_call_flags_narrate_no_act() {
        // thread 480b2469 msg3/5/9: model writes "let me find ..." / "让我先找到"
        // but emits no tool_use. These must be flagged so the recovery loop
        // nudges instead of ending the turn as a clean EndTurn.
        assert!(super::announces_tool_intent_without_call(
            "让我先找到相关组件代码"
        ));
        assert!(super::announces_tool_intent_without_call(
            "Let me search the codebase now."
        ));
        assert!(super::announces_tool_intent_without_call(
            "I'll call the Read tool."
        ));
        assert!(super::announces_tool_intent_without_call(
            "接下来我先看一下配置"
        ));
    }

    #[test]
    fn announces_tool_intent_without_call_skips_real_answers() {
        // A genuine long answer, or short text with no intent phrase, must not
        // be flagged — only narrate-without-acting trips the check.
        let long = "x".repeat(500);
        assert!(!super::announces_tool_intent_without_call(&long));
        // thread 480b2469 msg7: screenshot recap, no "让我", short-ish — must not trip.
        assert!(!super::announces_tool_intent_without_call(
            "我可以看到截图里有一个文本溢出的列表。"
        ));
        assert!(!super::announces_tool_intent_without_call("完成。"));
    }

    #[test]
    fn normalize_drops_whitespace_blocks_and_merges_adjacent_same_type() {
        use super::normalize_assistant_content;
        // thread 480b2469 msg3 shape: Thinking, Text, Thinking("."), Text —
        // the whitespace Thinking is dropped; the two Texts are NOT adjacent
        // (Thinking between them was non-empty? here it is whitespace, so after
        // dropping it the two Texts become adjacent and merge).
        let mut content = vec![
            MessageContent::Thinking {
                text: "real reasoning".into(),
                signature: Some("sig".into()),
            },
            MessageContent::Text("让我先找到相关的".into()),
            MessageContent::Thinking {
                text: "   ".into(),
                signature: None,
            },
            MessageContent::Text("组件代码".into()),
        ];
        normalize_assistant_content(&mut content);
        assert_eq!(
            content.len(),
            2,
            "whitespace thinking dropped, two texts merge"
        );
        assert!(
            matches!(&content[0], MessageContent::Thinking { text, signature } if text == "real reasoning" && signature.as_deref() == Some("sig"))
        );
        assert!(matches!(&content[1], MessageContent::Text(t) if t == "让我先找到相关的组件代码"));
    }

    #[test]
    fn normalize_keeps_thinking_separated_by_text_distinct() {
        use super::normalize_assistant_content;
        // Thinking blocks separated by a real Text block must NOT merge —
        // merging would erase the streaming timeline.
        let mut content = vec![
            MessageContent::Thinking {
                text: "first".into(),
                signature: Some("s1".into()),
            },
            MessageContent::Text("visible answer".into()),
            MessageContent::Thinking {
                text: "second".into(),
                signature: Some("s2".into()),
            },
        ];
        normalize_assistant_content(&mut content);
        assert_eq!(content.len(), 3, "text between thinking prevents merge");
        assert!(matches!(&content[0], MessageContent::Thinking { text, .. } if text == "first"));
        assert!(matches!(&content[1], MessageContent::Text(t) if t == "visible answer"));
        assert!(matches!(&content[2], MessageContent::Thinking { text, .. } if text == "second"));
    }

    #[test]
    fn normalize_merges_adjacent_thinking_retaining_first_signature() {
        use super::normalize_assistant_content;
        // Defensive backstop: accidentally-adjacent Thinking blocks merge, and
        // the first non-empty signature is retained (Anthropic echo requirement).
        let mut content = vec![
            MessageContent::Thinking {
                text: "a".into(),
                signature: Some("keep".into()),
            },
            MessageContent::Thinking {
                text: "b".into(),
                signature: Some("drop".into()),
            },
        ];
        normalize_assistant_content(&mut content);
        assert_eq!(content.len(), 1);
        match &content[0] {
            MessageContent::Thinking { text, signature } => {
                assert_eq!(text, "ab");
                assert_eq!(
                    signature.as_deref(),
                    Some("keep"),
                    "first signature retained"
                );
            }
            _ => panic!("expected merged Thinking"),
        }
    }

    // === plan-mode recovery regressions (thread cb80f55b) ===
    //
    // Drives `events` through a fresh depth-1 thread (sub-agent, so title
    // generation is skipped and the call counter stays clean) in either Plan
    // or Default mode, runs a single turn to quiescence, and returns the
    // stream-call count plus every User-role Text block appended during the
    // turn — so a test can assert which recovery path fired (Done vs. nudge)
    // and what the nudge actually said.
    fn run_recovery_case(
        thread_id: &str,
        model_id: &str,
        events: Vec<LanguageModelCompletionEvent>,
        plan_mode: bool,
    ) -> (u64, Vec<String>) {
        use crate::collaboration_mode::ModeKind;
        use crate::language_model::Role;
        use std::sync::atomic::{AtomicU64, Ordering};

        let _store_lock = THREAD_STORE_TEST_LOCK.lock().unwrap();
        crate::agent_def::init();
        let cx = gpui::TestAppContext::single();
        cx.update(|cx| {
            crate::runtime::init(cx);
        });

        struct StoreGuard;
        impl Drop for StoreGuard {
            fn drop(&mut self) {
                crate::thread_store::drop_for_test();
            }
        }
        let _store_guard = StoreGuard;

        let db = Arc::new(
            crate::db::ThreadsDatabase::open(std::path::Path::new(":memory:"))
                .expect("open mem db"),
        );
        cx.update(|cx| {
            crate::thread_store::init_for_test(db, cx);
        });

        let calls = Arc::new(AtomicU64::new(0));
        let model: AnyLanguageModel = Arc::new(CountingReplayMockModel {
            id: model_id.into(),
            events: Arc::new(events),
            calls: calls.clone(),
        });

        let thread = cx.update(|cx| {
            let mut rec = crate::db::ThreadRecord::for_test(
                thread_id,
                "/tmp",
                vec![Message::user("go".into())],
            );
            rec.title = Some("regression".into());
            let thread = super::Thread::restore(rec, Some(model), cx);
            thread.update(cx, |t, _| t.depth = 1);
            if plan_mode {
                thread.update(cx, |t, cx| t.set_collaboration_mode(ModeKind::Plan, cx));
            }
            thread
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
        cx.run_until_parked();

        let user_texts = cx.update(|cx| {
            thread.read_with(cx, |t, _| {
                t.messages
                    .iter()
                    .filter(|m| m.role == Role::User)
                    .flat_map(|m| {
                        m.content.iter().filter_map(|c| match c {
                            MessageContent::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                    })
                    .collect::<Vec<_>>()
            })
        });
        (calls.load(Ordering::SeqCst), user_texts)
    }

    #[test]
    fn produced_plan_this_turn_short_circuits_degenerate() {
        use crate::language_model::StopReason;
        // Plan mode, a full `<proposed_plan>` block (block content never
        // enters the assistant message), no tool_use, EndTurn. The block IS
        // the turn's intended output — recovery must end the turn (Done) and
        // emit PlanReady, not flag an empty visible-text turn as degenerate.
        let (calls, user_texts) = run_recovery_case(
            "reg-plan-block-done",
            "test/plan-block",
            vec![
                LanguageModelCompletionEvent::Text(
                    "<proposed_plan>step 1\nstep 2</proposed_plan>".into(),
                ),
                LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
            ],
            true,
        );
        assert_eq!(
            calls, 1,
            "a produced plan block must end the turn (Done), not retry"
        );
        assert!(
            !user_texts
                .iter()
                .any(|t| t.contains("produced no visible output")),
            "must not inject the empty-turn nudge when a plan block was produced"
        );
    }

    #[test]
    fn empty_turn_nudge_plan_mode_guides_to_proposed_plan() {
        use crate::language_model::StopReason;
        // Plan mode, empty stream (no visible text, no tool_use) → degenerate.
        // The nudge must steer toward `<proposed_plan>` and must NOT mention the
        // removed `exit_plan_mode` tool (thread cb80f55b's failure mode).
        let (calls, user_texts) = run_recovery_case(
            "reg-plan-empty-nudge",
            "test/plan-empty",
            vec![LanguageModelCompletionEvent::Stop(StopReason::EndTurn)],
            true,
        );
        assert!(
            calls > 1,
            "a degenerate empty turn must be retried, not silently end"
        );
        let nudge = user_texts
            .iter()
            .find(|t| t.contains("produced no visible output"))
            .expect("the empty-turn nudge must be injected");
        assert!(
            nudge.contains("<proposed_plan>"),
            "plan-mode nudge must guide to <proposed_plan>: {nudge}"
        );
        assert!(
            !nudge.contains("exit_plan_mode"),
            "plan-mode nudge must not reference the removed exit_plan_mode tool: {nudge}"
        );
    }

    #[test]
    fn empty_turn_nudge_default_mode_keeps_act_or_answer() {
        use crate::language_model::StopReason;
        // Default mode, empty stream → degenerate. The nudge keeps the
        // act-or-answer guidance and must not mention plan blocks.
        let (calls, user_texts) = run_recovery_case(
            "reg-default-empty-nudge",
            "test/default-empty",
            vec![LanguageModelCompletionEvent::Stop(StopReason::EndTurn)],
            false,
        );
        assert!(calls > 1, "degenerate empty turn must be retried");
        let nudge = user_texts
            .iter()
            .find(|t| t.contains("produced no visible output"))
            .expect("the empty-turn nudge must be injected");
        assert!(
            nudge.contains("tool_use"),
            "default-mode nudge must steer to acting: {nudge}"
        );
        assert!(
            !nudge.contains("exit_plan_mode"),
            "default nudge must not reference the removed tool: {nudge}"
        );
        assert!(
            !nudge.contains("<proposed_plan>"),
            "default-mode nudge must not mention plan blocks: {nudge}"
        );
    }

    #[test]
    fn plan_mode_skips_unfulfilled_tool_intent_nudge() {
        use crate::language_model::StopReason;
        // Plan mode, "让我制定修复计划" announces intent but emits no
        // tool_use and no plan block. Plan mode must NOT fire the
        // unfulfilled-tool-intent nudge (plan declarations are legitimate) —
        // the turn ends Done with the declaration visible to the user.
        let (calls, user_texts) = run_recovery_case(
            "reg-plan-unfulfilled-skip",
            "test/plan-unfulfilled",
            vec![
                LanguageModelCompletionEvent::Text("让我制定修复计划".into()),
                LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
            ],
            true,
        );
        assert_eq!(calls, 1, "plan mode must not retry on a plan declaration");
        assert!(
            !user_texts
                .iter()
                .any(|t| t.contains("announced a tool action in prose")),
            "plan mode must skip the unfulfilled-tool-intent nudge"
        );
    }

    #[test]
    fn default_mode_still_nudges_unfulfilled_intent() {
        use crate::language_model::StopReason;
        // Default mode, same declaration → the unfulfilled-tool-intent nudge
        // still fires (regression guard for the plan-mode exemption).
        let (calls, user_texts) = run_recovery_case(
            "reg-default-unfulfilled",
            "test/default-unfulfilled",
            vec![
                LanguageModelCompletionEvent::Text("让我制定修复计划".into()),
                LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
            ],
            false,
        );
        assert!(calls > 1, "default mode must retry on unfulfilled intent");
        assert!(
            user_texts
                .iter()
                .any(|t| t.contains("announced a tool action in prose")),
            "default mode must inject the unfulfilled-tool-intent nudge"
        );
    }
}
