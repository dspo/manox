// AgentHarness — orchestration layer.
//
// Wraps the agent loop with session persistence, hooks, compaction
// integration, and phase management. This is the primary public API
// for consumers of the harness.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::{
    AfterToolCallHook, Agent, BeforeProviderRequestHook, BeforeToolCallHook, EventMiddleware,
    LoopHooks, PrepareTurnHook, RunHandle,
};
use crate::agent_loop::StreamFn;
use crate::compaction::{self, CompactionPreparation, CompactionResult, CompactionSettings};
use crate::env::{ExecutionEnv, TokioExecutionEnv};
use crate::provider::retry;
use crate::session::{CompactionAuthorship, Session, SessionStorage, SessionTreeEntry};
use crate::tool::{AgentToolResult, LocalToolContext, ToolState};
use crate::types::{
    AgentContext, AgentEvent, AgentMessage, CacheRetention, ContentBlock, Model, StopReason, Usage,
};
use serde::Serialize;
use serde_json::Value as JsonValue;

/// The phases the harness can be in.
///
/// Structured operations (prompt, compact, retry) require the harness to
/// be in the Idle phase. Turn transitions happen internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentHarnessPhase {
    /// No active operation.
    Idle,
    /// Processing a user turn.
    Turn,
    /// Running compaction.
    Compaction,
    /// Generating a branch summary.
    BranchSummary,
    /// Retrying a failed operation.
    Retry,
}

/// Agent-level auto-retry policy for transient provider failures, mirroring
/// the TS `settings.retry` (`enabled` / `maxRetries` / `baseDelayMs`). The
/// initial call never counts as a retry; the per-attempt delay is
/// `baseDelayMs * 2^(attempt-1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Base delay in ms for the first retry.
    pub base_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        RetrySettings {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
        }
    }
}

/// Which model call a retry lifecycle belongs to.
///
/// Retries are scheduled from three unrelated places, and an observer that
/// cannot tell them apart cannot report or act on them differently — a failing
/// turn and a failing summarization call warrant different responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOperation {
    /// The agent turn itself.
    Turn,
    /// A compaction's summarization call.
    Compaction,
    /// A branch summary's summarization call.
    BranchSummary,
}

/// A retry lifecycle event, mirroring the TS `auto_retry_start` /
/// `auto_retry_end` session events.
#[derive(Debug, Clone)]
pub enum RetryEvent {
    /// A retry was scheduled: attempt `attempt` (1-indexed) retries the
    /// failed call after `delay`, up to `max_attempts`.
    Start {
        operation: RetryOperation,
        attempt: u32,
        max_attempts: u32,
        delay: std::time::Duration,
        error_message: String,
    },
    /// The scheduled backoff elapsed and the attempt is starting. A cancelled
    /// backoff goes straight from `Start` to `End` without this.
    AttemptStart {
        operation: RetryOperation,
        attempt: u32,
    },
    /// The retry lifecycle ended: `success` when a retry completed, otherwise
    /// the failure that exhausted the budget (or a cancellation) as
    /// `final_error`.
    End {
        operation: RetryOperation,
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
}

/// Whether a summarization error is transient and worth retrying (TS
/// `isRetryableAssistantError`): retryable HTTP statuses and transport
/// failures; auth, quota/billing, and invalid requests are not. A retryable
/// status whose body names quota/billing/credit is terminal — retrying a
/// billing failure only burns attempts.
fn is_transient_error(err: &anyhow::Error) -> bool {
    if let Some(pe) = err.downcast_ref::<crate::provider::ProviderError>() {
        return match pe {
            crate::provider::ProviderError::Http { status, body } => {
                crate::provider::retry::is_retryable_status(*status) && !is_quota_or_billing(body)
            }
            crate::provider::ProviderError::Transport(_) => true,
            _ => false,
        };
    }
    false
}

/// Whether an error body names quota or billing — terminal, never retried.
fn is_quota_or_billing(body: &str) -> bool {
    let lower = body.to_lowercase();
    [
        "quota",
        "billing",
        "credit",
        "insufficient",
        "payment",
        "plan limit",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// The shared runtime snapshot (model + thinking level) that new runs and
/// next-turn refreshes build their context from. Both the idle setters and
/// the mid-run handle setters update it immediately; the durable session
/// writes are queued separately.
#[derive(Clone)]
struct TurnRuntime {
    model: Model,
    thinking_level: Option<String>,
    active_tool_names: Option<Vec<String>>,
    /// Per-request provider options applied to every provider call.
    stream_options: crate::types::StreamOptions,
}

/// Shared harness control state exposed as [`AgentHarness::handle`], so a
/// caller can abort or await a harness that `prompt`/`continue_` own
/// exclusively — the retry backoff runs while the harness is `&mut`-borrowed.
struct HarnessControl {
    /// The current retry backoff token, or a cancelled one when no retry is
    /// in flight. Each retry arms a fresh token; [`HarnessHandle::abort`]
    /// and [`AgentHarness::abort`] fire the live one.
    retry_cancel: std::sync::Mutex<CancellationToken>,
    /// Number of operations (prompt/continue_/compact, including their
    /// settle phases) in flight; zero means the harness is fully settled.
    active_tx: watch::Sender<usize>,
    /// The runtime truth for following turns: model and thinking level as
    /// mutated by idle setters and mid-run handle setters alike. Every new
    /// run and every next-turn refresh reads it.
    turn_runtime: std::sync::Mutex<TurnRuntime>,
    /// Durable writes for mutations applied mid-run. Entries leave the queue
    /// only after their session append succeeds, so a failed write keeps the
    /// tail for the next flush.
    pending_mutations: std::sync::Mutex<Vec<PendingMutation>>,
    /// Messages the mid-run boundary made durable, each paired with the entry
    /// id it was written as, awaiting the transcript sync the harness performs
    /// once the run settles. The transcript otherwise grows only through
    /// `MessageEnd`, which these never produce, so this is also the only route
    /// by which their entry ids reach the index aligned with it.
    flushed_messages: std::sync::Mutex<Vec<(AgentMessage, String)>>,
    /// Messages queued by `next_turn`, prepended to the next prompt batch —
    /// the TS `nextTurnQueue`. Shared so the decoupled handle can enqueue
    /// mid-run without holding `&mut self`.
    next_turn_queue: std::sync::Mutex<Vec<AgentMessage>>,
    /// The cancellation token of the active structured operation
    /// (navigation), armed per operation. `abort` / `request_shutdown`
    /// cancel it so long-running branch summarization ends promptly.
    operation_cancel: std::sync::Mutex<CancellationToken>,
    /// The construction-time stream's api. Without a resolver, a model whose
    /// api differs is refused — the fixed stream cannot serve it.
    fixed_api: String,
    /// Whether a per-model resolver is plugged in; when set, any api is
    /// resolvable.
    has_resolver: std::sync::atomic::AtomicBool,
    /// Entry ids recorded by the persistence middleware for messages appended
    /// since the last drain, aligned with the transcript's new tail. The
    /// harness merges them into its own alignment after each run.
    message_entry_ids: std::sync::Mutex<Vec<Option<String>>>,
    /// Harness-level event listeners (queue updates, settled, mutations).
    /// Shared so the decoupled handle can emit without holding `&mut self`.
    harness_listeners: std::sync::Mutex<Vec<(u64, HarnessListener)>>,
    /// Source of listener ids, so a subscription can identify its own entry.
    next_harness_listener_id: std::sync::atomic::AtomicU64,
    /// Whether the mid-turn boundary drained any mutation since the last save
    /// point, so that save point reports the whole boundary's work.
    flushed_any_mutation: std::sync::atomic::AtomicBool,
    /// Whether shutdown was requested; all structured operations are refused
    /// while set.
    shutdown: std::sync::atomic::AtomicBool,
}

/// A runtime mutation queued mid-run; applied to the shared snapshot
/// immediately and persisted by the harness once the run settles.
#[derive(Clone)]
enum PendingMutation {
    Model(Model),
    ThinkingLevel(Option<String>),
    ActiveTools(Vec<String>),
    Message(AgentMessage),
}

/// Events a harness emits outside the agent run, mirroring the TS harness
/// `queue_update` / `settled` / `model_update` surface. Listeners are sync
/// callbacks fired in registration order at the moment the state changes.
#[derive(Debug, Clone)]
pub enum HarnessEvent {
    /// A queue changed: steering, follow-up, and next-turn counts. Fired on
    /// enqueue, drain, clear, and abort.
    QueueUpdate {
        steer: usize,
        follow_up: usize,
        next_turn: usize,
    },
    /// The whole operation (run + settle) settled.
    Settled { next_turn_count: usize },
    /// The model the next turn runs against changed.
    ModelUpdate { model: crate::types::Model },
    /// The reasoning tier changed.
    ThinkingLevelUpdate { level: Option<String> },
    /// The active tool selection changed.
    ToolsUpdate {
        active_tool_names: Option<Vec<String>>,
    },
    /// The mounted resources changed.
    ResourcesUpdate,
    /// Session writes are flushed: everything up to this point is durable, so
    /// a consumer tracking recoverable state can mark a checkpoint.
    SavePoint { had_pending_mutations: bool },
    /// A run was aborted, carrying the queued messages that were discarded so
    /// a consumer can put the user's unsent input back where it came from.
    Abort {
        cleared_steer: Vec<AgentMessage>,
        cleared_follow_up: Vec<AgentMessage>,
    },
}

/// A harness-level event listener (sync, fire-and-forget).
pub type HarnessListener = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

/// A harness event subscription. Dropping it unsubscribes, so a listener
/// cannot outlive the consumer that registered it.
pub struct HarnessSubscription {
    id: u64,
    control: Arc<HarnessControl>,
}

impl Drop for HarnessSubscription {
    fn drop(&mut self) {
        self.control
            .harness_listeners
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != self.id);
    }
}

/// Sets the harness's active count for the duration of an operation and
/// decrements it on drop, so a shared handle's `wait_for_idle` observes the
/// whole operation — agent run plus settle — rather than just the agent turn.
struct ActiveGuard {
    tx: watch::Sender<usize>,
}

impl ActiveGuard {
    fn arm(control: &HarnessControl) -> Self {
        let tx = control.active_tx.clone();
        tx.send_modify(|n| *n += 1);
        ActiveGuard { tx }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.tx.send_modify(|n| *n = n.saturating_sub(1));
    }
}

impl HarnessControl {
    fn emit_harness(&self, event: HarnessEvent) {
        let listeners = self.harness_listeners.lock().unwrap();
        for (_, listener) in listeners.iter() {
            listener(event.clone());
        }
    }

    fn emit_queue_counts(&self, steer: usize, follow_up: usize, next_turn: usize) {
        self.emit_harness(HarnessEvent::QueueUpdate {
            steer,
            follow_up,
            next_turn,
        });
    }
}

/// A decoupled handle for mid-run control of a harness, mirroring the TS
/// session's abort/waitForIdle surface: it reaches the agent run, the retry
/// backoff, and the full settle signal without holding `&mut self`, so a
/// caller can cancel or await a harness while `prompt`/`continue_` are in
/// flight.
#[derive(Clone)]
pub struct HarnessHandle {
    run: crate::agent::RunHandle,
    control: Arc<HarnessControl>,
}

impl HarnessHandle {
    /// Queue a steering message injected into the current or next turn.
    pub fn steer(&self, message: AgentMessage) {
        self.run.steer(message);
        self.control.emit_queue_counts(
            self.run.queued_steering_count(),
            self.run.queued_follow_up_count(),
            self.control.next_turn_queue.lock().unwrap().len(),
        );
    }

    /// Queue a follow-up message that resumes a run that would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        self.run.follow_up(message);
        self.control.emit_queue_counts(
            self.run.queued_steering_count(),
            self.run.queued_follow_up_count(),
            self.control.next_turn_queue.lock().unwrap().len(),
        );
    }

    /// Abort the agent run and cancel any in-flight retry backoff, clearing
    /// every queue (TS abort). Returns whether a run or backoff was active.
    ///
    /// The discarded queue contents ride on the emitted
    /// [`HarnessEvent::Abort`], so undelivered user input is recoverable.
    pub fn abort(&self) -> bool {
        self.run.abort();
        self.control.retry_cancel.lock().unwrap().cancel();
        self.control.operation_cancel.lock().unwrap().cancel();
        let (cleared_steer, cleared_follow_up) = self.run.clear_queues();
        self.control.next_turn_queue.lock().unwrap().clear();
        self.control.emit_harness(HarnessEvent::Abort {
            cleared_steer,
            cleared_follow_up,
        });
        true
    }

    /// Resolve once the harness is fully settled: no agent run, no retry
    /// backoff, no settle loop in flight.
    pub async fn wait_for_idle(&self) {
        let mut rx = self.control.active_tx.subscribe();
        while *rx.borrow_and_update() != 0 {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Queue a model change for the next turn boundary of the in-flight run.
    /// The shared runtime snapshot updates immediately — the next provider
    /// request and the next run both see it — and the change is persisted
    /// once the run settles (the TS mid-run `setModel`).
    pub fn set_model(&self, model: Model) {
        if !self
            .control
            .has_resolver
            .load(std::sync::atomic::Ordering::SeqCst)
            && !self.control.fixed_api.is_empty()
            && model.api != self.control.fixed_api
        {
            return;
        }
        self.control.turn_runtime.lock().unwrap().model = model.clone();
        self.control.emit_harness(HarnessEvent::ModelUpdate {
            model: model.clone(),
        });
        self.control
            .pending_mutations
            .lock()
            .unwrap()
            .push(PendingMutation::Model(model));
    }

    /// Queue a thinking-level change for the next turn boundary of the
    /// in-flight run, same semantics as [`HarnessHandle::set_model`].
    pub fn set_thinking_level(&self, level: Option<String>) {
        self.control.turn_runtime.lock().unwrap().thinking_level = level.clone();
        self.control
            .emit_harness(HarnessEvent::ThinkingLevelUpdate {
                level: level.clone(),
            });
        self.control
            .pending_mutations
            .lock()
            .unwrap()
            .push(PendingMutation::ThinkingLevel(level));
    }

    /// Queue an active-tools change for the next turn boundary of the
    /// in-flight run. The runtime snapshot applies it to the next provider
    /// request's context; the harness persists it once the run settles.
    pub fn set_active_tools(&self, active_tool_names: Vec<String>) {
        self.control.turn_runtime.lock().unwrap().active_tool_names =
            Some(active_tool_names.clone());
        self.control.emit_harness(HarnessEvent::ToolsUpdate {
            active_tool_names: Some(active_tool_names.clone()),
        });
        self.control
            .pending_mutations
            .lock()
            .unwrap()
            .push(PendingMutation::ActiveTools(active_tool_names));
    }

    /// Queue per-request provider options for the next turn boundary of the
    /// in-flight run. Unlike the durable mutations, stream options are
    /// ephemeral: the snapshot updates immediately and the next
    /// `apply_turn_runtime` forwards them into every provider request.
    pub fn set_stream_options(&self, options: crate::types::StreamOptions) {
        self.control.turn_runtime.lock().unwrap().stream_options = options;
    }

    /// Begin shutdown from mid-run: stop accepting new operations, cancel
    /// the active run and any retry backoff, clear every queue and the
    /// unpersisted mutation queue. Idempotent.
    pub fn request_shutdown(&self) {
        if self
            .control
            .shutdown
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.run.abort();
        self.control.retry_cancel.lock().unwrap().cancel();
        self.control.operation_cancel.lock().unwrap().cancel();
        let (cleared_steer, cleared_follow_up) = self.run.clear_queues();
        self.control.next_turn_queue.lock().unwrap().clear();
        self.control.pending_mutations.lock().unwrap().clear();
        self.control.emit_harness(HarnessEvent::Abort {
            cleared_steer,
            cleared_follow_up,
        });
    }

    /// Queue a user message for the next prompt batch — the TS mid-run
    /// `nextTurn`. Unlike [`AgentHarness::next_turn`], this works while a
    /// run is in flight: the message lands in the shared queue and the next
    /// prompt prepends it before its own message.
    pub fn next_turn(&self, text: &str, images: Vec<ContentBlock>) {
        let mut content = Vec::with_capacity(images.len() + 1);
        content.push(ContentBlock::Text {
            text: text.to_string(),
            signature: None,
        });
        content.extend(images);
        self.control
            .next_turn_queue
            .lock()
            .unwrap()
            .push(AgentMessage::User {
                content,
                timestamp: chrono::Utc::now(),
            });
    }

    /// Record a message produced outside the turn while a run is in flight.
    ///
    /// The message is held until the run reaches a turn boundary, so it lands
    /// after that turn's own messages rather than between a tool call and its
    /// result.
    pub fn append_message(&self, message: AgentMessage) {
        self.control
            .pending_mutations
            .lock()
            .unwrap()
            .push(PendingMutation::Message(message));
    }
}

/// Hook points that consumers can register handlers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// Before the agent starts processing a turn.
    BeforeAgentStart,
    /// Before the context is sent to the provider.
    BeforeProviderRequest,
    /// When a tool call is about to execute.
    ToolCall,
    /// After a tool result is received.
    ToolResult,
    /// Before the session is compacted.
    SessionBeforeCompact,
    /// After the session is compacted.
    SessionAfterCompact,
    /// Before the session tree cursor moves (tree navigation).
    SessionBeforeTree,
    /// After the session tree cursor moved.
    SessionTree,
    /// The provider request payload is about to be sent (fires per attempt).
    BeforeProviderPayload,
    /// The provider responded with a status (fires per attempt).
    AfterProviderResponse,
}

/// A hook handler receives context about the event and can mutate it.
pub type HookHandler = Arc<dyn Fn(HookContext) -> HookContext + Send + Sync>;

/// The typed `before_agent_start` hook event, mirroring the TS
/// `BeforeAgentStartEvent`. Serialized into [`HookContext::data`] so a handler
/// receives the TS-shaped fields rather than an ad-hoc payload. TS also
/// carries `images` and `resources`; neither exists on the Rust harness (no
/// image prompt input, no resource registry), so the payload omits them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEvent<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub prompt: &'a str,
    pub system_prompt: &'a str,
}

/// A before-compact hook's full compaction result, mirroring the TS
/// `CompactResult`. Persisted verbatim (`fromHook = true`) — the harness does
/// not fall back to its own cut analysis on any field, so a TS hook returning
/// a `CompactResult` migrates without behavioral drift. `summary`,
/// `tokens_before`, and `retained_tail` are required (the hook owns the full
/// result); an empty summary is refused before persisting. A hook that wants
/// to keep the harness-computed tail passes through `preparation.retained_tail`
/// — supplying `vec![]` deliberately erases it. The remaining optional fields
/// (`firstKeptEntryId`, `usage`, `details`) are persisted as supplied — `None`
/// means absent, not "compute for me".
///
/// `Default` is intentionally not derived: it would let a summary-only override
/// silently erase the retained tail and report `tokens_before: 0`, which is
/// exactly the partial-override footgun the full-result contract rules out.
/// Every field must be set explicitly at the construction site.
#[derive(Debug, Clone)]
pub struct BeforeCompactOverride {
    pub summary: String,
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    pub usage: Option<Usage>,
    pub retained_tail: Vec<AgentMessage>,
    pub details: Option<JsonValue>,
}

/// The typed `session_before_compact` hook event, mirroring the TS
/// `SessionBeforeCompactEvent`. Serialized into [`HookContext::data`] so a
/// handler receives the TS-shaped `preparation` and `branchEntries` rather
/// than an ad-hoc payload. The TS `signal: AbortSignal` has no Rust sync-hook
/// equivalent — the hook is a synchronous closure with nothing to observe —
/// and cancellation is expressed the other direction via the result's
/// `cancel` field ([`HookContext::with_cancel_compaction`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeCompactEvent<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub preparation: &'a CompactionPreparation,
    pub branch_entries: &'a [SessionTreeEntry],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<&'a str>,
}

/// A branch summary supplied by the `session_before_tree` hook, mirroring
/// the TS `summary` override. Persisted verbatim with `fromHook`.
#[derive(Debug, Clone)]
pub struct BranchSummaryHookOverride {
    pub summary: String,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub usage: Option<crate::types::Usage>,
}

/// Rebuilds the harness system prompt from the effective tool selection and
/// resources.
pub type SystemPromptBuilder = Arc<dyn Fn(&[String], &HarnessResources) -> String + Send + Sync>;

/// The typed `session_before_tree` hook event, mirroring the TS
/// `SessionBeforeTreeEvent`: the navigation preparation a handler can cancel
/// or use to override the summarization instructions and label. The TS
/// `signal: AbortSignal` has no Rust sync-hook equivalent — cancellation is
/// expressed via the result's `cancel` field
/// ([`HookContext::with_cancel_tree`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeEvent<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub target_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_leaf_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub common_ancestor_id: Option<&'a str>,
    pub entries_to_summarize: &'a [SessionTreeEntry],
    pub user_wants_summary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<&'a str>,
    pub replace_instructions: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'a str>,
}

/// The typed `session_tree` hook event fired after the cursor moved,
/// mirroring the TS `SessionTreeEvent`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeEvent<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_leaf_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_leaf_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_entry_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Context passed to hook handlers.
///
/// Handlers return a (possibly mutated) copy; the harness threads selected
/// fields back into the loop. `agent_context` feeds the provider request,
/// `block_reason` gates a tool call, `tool_result` patches a tool result,
/// `cancel_compaction`/`compact_override` steer the compaction flow,
/// `cancel_tree`/`tree_custom_instructions`/`tree_label` steer tree
/// navigation, and `inject_messages`/`system_prompt_override` carry the
/// `before_agent_start` effects.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// The hook point being triggered.
    pub hook: HookPoint,
    /// The current agent context (messages, model, etc.).
    pub agent_context: Option<AgentContext>,
    /// Arbitrary data attached to the hook event.
    pub data: serde_json::Value,
    /// `Some(reason)` at the `ToolCall` point blocks the call before it runs.
    pub block_reason: Option<String>,
    /// A replacement `AgentToolResult` at the `ToolResult` point; when set it
    /// supplants the result the tool produced.
    pub tool_result: Option<AgentToolResult>,
    /// At the `SessionBeforeCompact` point, aborts the compaction without
    /// persisting anything.
    pub cancel_compaction: bool,
    /// At the `SessionBeforeCompact` point, supplies the summary directly so
    /// the harness skips the summarization model call.
    pub compact_override: Option<BeforeCompactOverride>,
    /// At the `BeforeAgentStart` point, extra messages appended to the prompt
    /// batch after the user message; they enter the transcript and session
    /// like any prompt message.
    pub inject_messages: Vec<AgentMessage>,
    /// At the `BeforeAgentStart` point, the system prompt the run's initial
    /// context carries. Only that first context sees the override — steering
    /// and follow-up turns snapshot the agent's configured prompt again.
    pub system_prompt_override: Option<String>,
    /// At the `SessionBeforeTree` point, aborts the navigation before any
    /// cursor move or entry append.
    pub cancel_tree: bool,
    /// At the `SessionBeforeTree` point, overrides the summarization custom
    /// instructions, label, and replace-instructions flag when present.
    pub tree_custom_instructions: Option<String>,
    pub tree_label: Option<String>,
    pub tree_replace_instructions: Option<bool>,
    /// At the `SessionBeforeTree` point, supplies the branch summary
    /// directly so the harness skips the summarization model call (TS
    /// `summary` with `fromHook`).
    pub tree_summary: Option<BranchSummaryHookOverride>,
}

impl HookContext {
    pub fn new(hook: HookPoint) -> Self {
        HookContext {
            hook,
            agent_context: None,
            data: serde_json::Value::Null,
            block_reason: None,
            tool_result: None,
            cancel_compaction: false,
            compact_override: None,
            inject_messages: Vec::new(),
            system_prompt_override: None,
            cancel_tree: false,
            tree_custom_instructions: None,
            tree_label: None,
            tree_replace_instructions: None,
            tree_summary: None,
        }
    }

    pub fn with_context(mut self, ctx: AgentContext) -> Self {
        self.agent_context = Some(ctx);
        self
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }

    pub fn with_tool_result(mut self, result: AgentToolResult) -> Self {
        self.tool_result = Some(result);
        self
    }

    pub fn with_block_reason(mut self, reason: impl Into<String>) -> Self {
        self.block_reason = Some(reason.into());
        self
    }

    /// At `SessionBeforeCompact`, cancel the compaction without persisting.
    pub fn with_cancel_compaction(mut self) -> Self {
        self.cancel_compaction = true;
        self
    }

    /// At `SessionBeforeCompact`, supply the summary directly and skip the
    /// summarization model call. The persisted entry carries `fromHook`.
    pub fn with_compact_override(mut self, override_: BeforeCompactOverride) -> Self {
        self.compact_override = Some(override_);
        self
    }

    /// At `SessionBeforeTree`, cancel the navigation before any cursor move.
    pub fn with_cancel_tree(mut self) -> Self {
        self.cancel_tree = true;
        self
    }

    /// At `SessionBeforeTree`, override the summarization custom instructions.
    pub fn with_tree_instructions(mut self, instructions: String) -> Self {
        self.tree_custom_instructions = Some(instructions);
        self
    }

    /// At `SessionBeforeTree`, override the label written to the tree.
    pub fn with_tree_label(mut self, label: String) -> Self {
        self.tree_label = Some(label);
        self
    }

    /// At `SessionBeforeTree`, override the replace-instructions flag.
    pub fn with_tree_replace_instructions(mut self, replace: bool) -> Self {
        self.tree_replace_instructions = Some(replace);
        self
    }

    /// At `SessionBeforeTree`, supply the branch summary directly; the
    /// harness skips the model call and persists it as a hook-authored
    /// entry (`fromHook`).
    pub fn with_tree_summary(mut self, summary: BranchSummaryHookOverride) -> Self {
        self.tree_summary = Some(summary);
        self
    }

    /// At `BeforeAgentStart`, append extra messages to the prompt batch,
    /// after the user message.
    pub fn with_inject_messages(mut self, messages: Vec<AgentMessage>) -> Self {
        self.inject_messages = messages;
        self
    }

    /// At `BeforeAgentStart`, replace the system prompt the run's initial
    /// context carries. The override does not stick: the next turn snapshots
    /// the agent's configured prompt again.
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt_override = Some(system_prompt.into());
        self
    }
}

/// Consumer-plugged registry lookup resolving a session-carried model
/// reference into a concrete model. Returning `None` leaves the
/// construction-time model in place on restore.
pub type ModelResolver =
    Arc<dyn Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync>;

/// The orchestration layer wrapping the agent loop.
pub struct AgentHarness<S: SessionStorage> {
    agent: Agent,
    session: Arc<Session<S>>,
    model: Model,
    phase: AgentHarnessPhase,
    compaction_settings: CompactionSettings,
    /// Timestamp of the latest compaction. Usage recorded at or before it
    /// measured a different message prefix, so it never anchors token
    /// estimates for the rewritten transcript. Recovered from the persisted
    /// session by [`AgentHarness::recover_boundary`].
    last_compaction_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Registered hook handlers, grouped by point. Shared via
    /// `Arc<Mutex<..>>` so the closures cloned into the agent's loop config
    /// read the live registration list — `on()` calls after construction
    /// still fire.
    hooks: Arc<Mutex<Vec<(HookPoint, HookHandler)>>>,
    /// The streaming function used for the summarization call in `compact()`.
    stream_fn: Arc<dyn StreamFn>,
    /// Per-model provider runtime resolution, when the consumer plugs one in;
    /// forwards to the agent for per-turn resolution and serves the
    /// summarization call in `compact()`.
    stream_resolver: Option<crate::agent_loop::StreamResolver>,
    /// Entry ids aligned by index with the agent transcript. `None` marks a
    /// synthetic carrier (a compaction summary message, or a message folded
    /// into a compaction entry's retained tail on restore) that has no
    /// standalone `Message` entry. Drives the real `first_kept_entry_id`
    /// recorded by `compact()`.
    message_entry_ids: Vec<Option<String>>,
    /// The one-shot budget for overflow recovery: set when an overflow turn
    /// was compacted and retried, cleared by a new user prompt or any
    /// non-error assistant message. Mirrors the TS
    /// `_overflowRecoveryAttempted` flag — a context that stays oversized
    /// after one compact-and-retry surfaces its error instead of looping.
    overflow_recovery_attempted: bool,
    /// Auto-retry policy for transient provider failures.
    retry_settings: RetrySettings,
    /// Attempts used by the current auto-retry lifecycle; reset by any
    /// non-error assistant message. Mirrors TS `_retryAttempt`.
    retry_attempt: u32,
    /// Shared mid-run control: the live retry backoff token and the active
    /// operation count behind [`AgentHarness::handle`].
    control: Arc<HarnessControl>,
    /// Observer for the auto-retry lifecycle, mirroring TS `_emit` of
    /// `auto_retry_start` / `auto_retry_end`.
    retry_observer: Option<Arc<dyn Fn(RetryEvent) + Send + Sync>>,
    /// Every mounted tool, including ones the active selection currently
    /// hides from the model. The agent receives only the active subset.
    all_tools: Arc<[Arc<dyn crate::tool::AgentTool>]>,
    /// The tool subset the model sees, from the latest explicit selection
    /// (persisted as an `active_tools_change` entry); `None` mounts the full
    /// set.
    active_tool_names: Option<Vec<String>>,
    /// Resolves the model reference a restored session path carries. The
    /// crate stays registry-free, so the consumer plugs in its registry;
    /// without one a restore keeps the construction-time model.
    model_resolver: Option<ModelResolver>,
    /// The active-tool subset a restore uses when the path carries no
    /// `active_tools_change` entry — the facade's default four tools. `None`
    /// restores the full registry (TS default).
    restore_active_tool_default: Option<Vec<String>>,
    /// Whether cache-miss notices are shown in the transcript (TS
    /// `showCacheMissNotices`; the harness records it for UI consumers).
    show_cache_miss_notices: bool,
    /// Tokens reserved for a branch summary's prompt + response (TS
    /// `branchSummary.reserveTokens`).
    branch_summary_reserve: usize,
    /// Consumer-provided system-prompt builder invoked whenever the
    /// effective active-tool selection or resources change, so the prompt
    /// never advertises tools/skills that are not actually available.
    system_prompt_builder: Option<SystemPromptBuilder>,
    /// Skills and prompt templates the harness can expand into prompts.
    resources: HarnessResources,
}

/// Options for [`AgentHarness::navigate_tree_with_options`], mirroring the
/// TS `navigateTree` options. `summarize` gates branch summarization
/// (off by default, like TS); `custom_instructions` / `replace_instructions`
/// shape the summarization prompt; `label` is written to the summary entry
/// when one is generated, otherwise to the target entry, and is also
/// surfaced on the `session_before_tree` hook (see docs/ts-pi-parity.md §9).
#[derive(Debug, Clone, Default)]
pub struct NavigateTreeOptions {
    /// Generate a summary of the abandoned branch (requires a provider).
    pub summarize: bool,
    /// Instructions appended to the summarization prompt.
    pub custom_instructions: Option<String>,
    /// Replace the default summarization instructions with
    /// `custom_instructions` instead of appending.
    pub replace_instructions: bool,
    /// Label carried for the deferred `session_before_tree` hook.
    pub label: Option<String>,
}

/// Result of a tree navigation, mirroring the TS `NavigateTreeResult`.
#[derive(Debug, Clone)]
pub struct NavigateTreeResult {
    /// True when the navigation was cancelled before moving the cursor: a
    /// `session_before_tree` hook cancellation or an aborted summarization.
    pub cancelled: bool,
    /// True when the summarization was aborted mid-flight; implies
    /// `cancelled`.
    pub aborted: bool,
    /// The target message's text when it is a user or custom message;
    /// `None` for assistant or structural targets.
    pub editor_text: Option<String>,
    /// Entry id of the appended branch summary, when one was generated.
    pub summary_entry_id: Option<String>,
}

impl<S: SessionStorage + 'static> AgentHarness<S> {
    /// Create a new harness with the given session, model, and stream function.
    pub fn new(
        session: Session<S>,
        system_prompt: impl Into<String>,
        model: Model,
        stream_fn: Arc<dyn StreamFn>,
    ) -> Self {
        // Build the session-scoped execution context once and share it across
        // every turn: a real env + cwd + ToolState so fs/shell tools work
        // instead of panicking on `env()`.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let env: Arc<dyn ExecutionEnv> = Arc::new(TokioExecutionEnv::new(cwd.clone()));
        let tool_state = Arc::new(ToolState::new());
        let tool_ctx: Arc<dyn crate::tool::ToolContext> =
            Arc::new(LocalToolContext::new(env, cwd, tool_state));
        let hooks: Arc<Mutex<Vec<(HookPoint, HookHandler)>>> = Arc::new(Mutex::new(Vec::new()));
        let fixed_api = stream_fn.api().to_string();
        let control: Arc<HarnessControl> = Arc::new(HarnessControl {
            retry_cancel: std::sync::Mutex::new(CancellationToken::new()),
            active_tx: watch::Sender::new(0),
            turn_runtime: std::sync::Mutex::new(TurnRuntime {
                model: model.clone(),
                thinking_level: None,
                active_tool_names: None,
                stream_options: crate::types::StreamOptions::default(),
            }),
            pending_mutations: std::sync::Mutex::new(Vec::new()),
            flushed_messages: std::sync::Mutex::new(Vec::new()),
            next_turn_queue: std::sync::Mutex::new(Vec::new()),
            harness_listeners: std::sync::Mutex::new(Vec::new()),
            next_harness_listener_id: std::sync::atomic::AtomicU64::new(0),
            flushed_any_mutation: std::sync::atomic::AtomicBool::new(false),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            operation_cancel: std::sync::Mutex::new(CancellationToken::new()),
            fixed_api,
            has_resolver: std::sync::atomic::AtomicBool::new(false),
            message_entry_ids: std::sync::Mutex::new(Vec::new()),
        });
        let mut agent = Agent::new(
            system_prompt,
            model.clone(),
            Arc::clone(&stream_fn),
            tool_ctx,
        );
        // The persistence middleware shares the session Arc so it can append
        // each message at MessageEnd; the harness keeps the other handle.
        let session = Arc::new(session);
        agent.set_loop_hooks(build_loop_hooks(
            Arc::clone(&hooks),
            Arc::clone(&control),
            Arc::clone(&session),
        ));
        agent.add_middleware(build_persistence_middleware(
            Arc::clone(&control),
            Arc::clone(&session),
        ));
        AgentHarness {
            agent,
            session,
            model,
            phase: AgentHarnessPhase::Idle,
            compaction_settings: CompactionSettings::default(),
            last_compaction_at: None,
            hooks,
            stream_fn,
            stream_resolver: None,
            message_entry_ids: Vec::new(),
            overflow_recovery_attempted: false,
            retry_settings: RetrySettings::default(),
            retry_attempt: 0,
            control,
            retry_observer: None,
            all_tools: Arc::from(Vec::new()),
            active_tool_names: None,
            model_resolver: None,
            resources: HarnessResources::default(),
            show_cache_miss_notices: false,
            branch_summary_reserve: crate::compaction::branch_summarization::RESERVE_TOKENS,
            system_prompt_builder: None,
            restore_active_tool_default: None,
        }
    }

    /// Run tools against `cwd` instead of the process working directory —
    /// the session's project directory. Rebuilds the execution environment
    /// and tool context so read/bash/grep/find/ls resolve relative paths
    /// there.
    pub fn with_tool_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        let env: Arc<dyn ExecutionEnv> = Arc::new(TokioExecutionEnv::new(cwd.clone()));
        let tool_state = Arc::new(ToolState::new());
        let tool_ctx: Arc<dyn crate::tool::ToolContext> =
            Arc::new(LocalToolContext::new(env, cwd, tool_state));
        self.agent.set_tool_ctx(tool_ctx);
        self
    }

    /// Mount tools on the underlying agent.
    ///
    /// The harness keeps the full set; the agent receives only the active
    /// subset (all of them until [`AgentHarness::set_active_tools`] narrows
    /// the selection).
    pub fn with_tools(mut self, tools: Arc<[Arc<dyn crate::tool::AgentTool>]>) -> Self {
        self.all_tools = tools;
        self.apply_active_tools();
        self
    }

    /// Plug in the registry lookup used to resolve the model reference a
    /// restored session carries. Without a resolver, restore keeps the
    /// construction-time model.
    pub fn with_model_resolver(
        mut self,
        resolver: impl Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync + 'static,
    ) -> Self {
        self.model_resolver = Some(Arc::new(resolver));
        self
    }

    /// Mount skills and prompt templates the harness expands into prompts.
    pub fn with_resources(mut self, resources: HarnessResources) -> Self {
        self.resources = resources;
        self
    }

    /// The mounted resources.
    pub fn resources(&self) -> &HarnessResources {
        &self.resources
    }

    /// Run a skill by name: its content becomes the prompt for this turn.
    pub async fn skill(&mut self, name: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let skill = self
            .resources
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown skill: {name}"))?;
        self.prompt(&format_skill_invocation(skill, None)).await
    }

    /// Expand a prompt template by name with `args` substituted for its
    /// `$1`/`$@`/`$ARGUMENTS` placeholders (see [`substitute_args`]), then
    /// run it.
    pub async fn prompt_from_template(
        &mut self,
        name: &str,
        args: &[String],
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let template = self
            .resources
            .prompt_templates
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.content.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown prompt template: {name}"))?;
        let rendered = substitute_args(&template, args);
        self.prompt(&rendered).await
    }

    /// Queue a user message for the next turn — the TS `nextTurn`. It can be
    /// called while idle or mid-run (via [`HarnessHandle::next_turn`]); the
    /// queued messages are prepended to the next prompt batch, before the
    /// prompt's own message.
    pub fn next_turn(&self, text: &str, images: Vec<ContentBlock>) {
        let mut content = Vec::with_capacity(images.len() + 1);
        content.push(ContentBlock::Text {
            text: text.to_string(),
            signature: None,
        });
        content.extend(images);
        self.control
            .next_turn_queue
            .lock()
            .unwrap()
            .push(AgentMessage::User {
                content,
                timestamp: chrono::Utc::now(),
            });
        self.emit_queue_update();
    }

    /// Whether next-turn messages are queued.
    pub fn has_next_turn(&self) -> bool {
        !self.control.next_turn_queue.lock().unwrap().is_empty()
    }

    /// Append a custom message to the session, joining the transcript.
    pub async fn add_custom_message(
        &self,
        custom_type: &str,
        content: Vec<ContentBlock>,
        display: bool,
    ) -> Result<String, anyhow::Error> {
        self.session
            .append_custom_message(custom_type, content, None, display)
            .await
    }

    /// Attach a label to an entry in the tree.
    pub async fn add_label(
        &self,
        target_id: &str,
        label: Option<String>,
    ) -> Result<String, anyhow::Error> {
        self.session.append_label(target_id, label).await
    }

    /// Set the session display name.
    pub async fn set_session_name(&self, name: &str) -> Result<String, anyhow::Error> {
        self.session.set_session_name(name).await
    }

    /// Re-resolve the current model's stream under a new resolver and swap it
    /// in — used to attach the request observer after the harness exists
    /// (the observer needs the harness's hook registry).
    pub fn rebind_stream_resolver(&mut self, resolver: crate::agent_loop::StreamResolver) {
        if let Ok(stream) = resolver(&self.model) {
            self.stream_fn = Arc::clone(&stream);
            self.stream_resolver = Some(resolver.clone());
            self.agent.set_stream_resolver(resolver);
            self.agent.set_stream_fn(stream);
        }
    }

    /// Plug in per-model provider runtime resolution (the consumer's registry
    /// seam — the crate stays registry-free). Every provider call — normal
    /// turns, overflow retries, continuations, and summarization — resolves
    /// its stream function from the current model, so a mid-session model
    /// change switches protocol/endpoint/credentials.
    pub fn with_stream_resolver(mut self, resolver: crate::agent_loop::StreamResolver) -> Self {
        self.stream_resolver = Some(Arc::clone(&resolver));
        self.control
            .has_resolver
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.agent.set_stream_resolver(resolver);
        self
    }

    /// Current phase.
    pub fn phase(&self) -> AgentHarnessPhase {
        self.phase
    }

    /// The current model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Access the underlying session storage.
    pub fn session(&self) -> &Session<S> {
        self.session.as_ref()
    }

    /// Access the agent.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Mutable access to the agent.
    pub fn agent_mut(&mut self) -> &mut Agent {
        &mut self.agent
    }

    /// Decoupled handle for mid-run control (steer/follow_up/abort).
    pub fn run_handle(&self) -> RunHandle {
        self.agent.run_handle()
    }

    /// Build a provider request observer that maps the per-attempt payload
    /// and status onto the [`HookPoint::BeforeProviderPayload`] /
    /// [`HookPoint::AfterProviderResponse`] hooks. Attach it to a provider
    /// stream builder (`with_request_observer`) so the harness's registered
    /// handlers see the wire traffic of every attempt.
    pub fn request_observer(&self) -> Arc<dyn crate::provider::RequestObserver> {
        struct Observer(Arc<Mutex<Vec<(HookPoint, HookHandler)>>>);
        impl crate::provider::RequestObserver for Observer {
            fn before_payload(
                &self,
                attempt: u32,
                model: &crate::types::Model,
                payload: &serde_json::Value,
            ) -> Option<serde_json::Value> {
                // Handlers chain: each receives the previous handler's
                // payload (the original when none has run yet), and its
                // returned `payload` becomes the next input — TS
                // before-payload composition.
                let mut current: Option<serde_json::Value> = None;
                let ctx_base = HookContext::new(HookPoint::BeforeProviderPayload);
                let list = self.0.lock().unwrap();
                for (point, handler) in list.iter() {
                    if *point == HookPoint::BeforeProviderPayload {
                        let effective = current.clone().unwrap_or_else(|| payload.clone());
                        let ctx = ctx_base.clone().with_data(serde_json::json!({
                            "attempt": attempt,
                            "model": { "provider": model.provider, "id": model.id, "api": model.api },
                            "payload": effective,
                        }));
                        let next = handler(ctx);
                        if let Some(data) = next.data.get("payload") {
                            current = Some(data.clone());
                        }
                    }
                }
                current
            }
            fn after_response(
                &self,
                attempt: u32,
                status: u16,
                headers: &reqwest::header::HeaderMap,
            ) {
                // TS `Record<string,string>`: header name -> value.
                let headers_json: serde_json::Map<String, serde_json::Value> = headers
                    .iter()
                    .filter_map(|(name, value)| {
                        let value = value.to_str().ok()?.to_string();
                        Some((name.as_str().to_string(), serde_json::Value::String(value)))
                    })
                    .collect();
                let ctx = HookContext::new(HookPoint::AfterProviderResponse).with_data(
                    serde_json::json!({ "attempt": attempt, "status": status, "headers": headers_json }),
                );
                let list = self.0.lock().unwrap();
                for (point, handler) in list.iter() {
                    if *point == HookPoint::AfterProviderResponse {
                        let _ = handler(ctx.clone());
                    }
                }
            }
        }
        Arc::new(Observer(Arc::clone(&self.hooks)))
    }

    /// Decoupled harness-level handle: queues, the agent run, the retry
    /// backoff, and the full settle signal. Unlike [`AgentHarness::run_handle`],
    /// its `abort` cancels the retry backoff and its `wait_for_idle` resolves
    /// only after the whole operation (run + settle) settles.
    pub fn handle(&self) -> HarnessHandle {
        HarnessHandle {
            run: self.agent.run_handle(),
            control: Arc::clone(&self.control),
        }
    }

    /// Current compaction settings.
    pub fn compaction_settings(&self) -> &CompactionSettings {
        &self.compaction_settings
    }

    /// Update compaction settings.
    pub fn set_compaction_settings(&mut self, settings: CompactionSettings) {
        self.compaction_settings = settings;
    }

    /// Current auto-retry policy.
    pub fn retry_settings(&self) -> &RetrySettings {
        &self.retry_settings
    }

    /// Update the auto-retry policy.
    pub fn set_retry_settings(&mut self, settings: RetrySettings) {
        self.retry_settings = settings;
    }

    /// Apply an initial thinking tier in memory — both the agent state and
    /// the shared runtime snapshot — without persisting an entry. Used for
    /// the settings default; `set_thinking_level` persists, this one does
    /// not.
    pub fn set_initial_thinking_level(&mut self, level: Option<String>) {
        // The in-memory tier is `None` for off (agent semantics); the wire
        // value "off" normalizes here so a first turn equals a reopen.
        let normalized = level.filter(|l| l != "off");
        self.agent.set_thinking_level(normalized.clone());
        self.control.turn_runtime.lock().unwrap().thinking_level = normalized;
    }

    /// Install the system-prompt builder; the harness re-invokes it whenever
    /// the effective tool selection or resources change.
    pub fn set_system_prompt_builder(
        &mut self,
        builder: impl Fn(&[String], &HarnessResources) -> String + Send + Sync + 'static,
    ) {
        self.system_prompt_builder = Some(Arc::new(builder));
    }

    /// Rebuild the system prompt from the builder over the EFFECTIVE tool
    /// selection and resources. A no-op without a builder (the harness keeps
    /// its construction-time prompt).
    fn rebuild_system_prompt(&mut self) {
        let Some(builder) = &self.system_prompt_builder else {
            return;
        };
        let names: Vec<String> = match &self.active_tool_names {
            Some(names) => names.clone(),
            None => self
                .all_tools
                .iter()
                .map(|t| t.name().to_string())
                .collect(),
        };
        let prompt = builder(&names, &self.resources);
        self.agent.set_system_prompt(prompt);
    }

    /// Set the initial active tool subset in memory (no `active_tools_change`
    /// entry) — used for the facade's TS default four tools.
    pub fn set_initial_active_tools(&mut self, names: Vec<String>) {
        self.active_tool_names = Some(names);
        self.apply_active_tools();
    }

    /// The active-tool subset a restore falls back to when the path carries
    /// no `active_tools_change` entry — the facade's default four tools, so
    /// a reopened default session does not drift to the full registry.
    pub fn set_restore_active_tool_default(&mut self, names: Vec<String>) {
        self.restore_active_tool_default = Some(names);
    }

    /// Whether cache-miss notices are shown.
    pub fn show_cache_miss_notices(&self) -> bool {
        self.show_cache_miss_notices
    }

    /// Set whether cache-miss notices are shown.
    pub fn set_show_cache_miss_notices(&mut self, enabled: bool) {
        self.show_cache_miss_notices = enabled;
    }

    /// Tokens reserved for a branch summary's prompt + response.
    pub fn branch_summary_reserve(&self) -> usize {
        self.branch_summary_reserve
    }

    /// Set the branch-summary reserve tokens (TS `branchSummary.reserveTokens`).
    pub fn set_branch_summary_reserve(&mut self, reserve: usize) {
        self.branch_summary_reserve = reserve;
    }

    /// Set the per-request provider options (headers, timeout, output
    /// budget) applied to every provider call from the next turn onward.
    /// Ephemeral runtime config: never persisted to the session, so a
    /// reopen restores the construction-time defaults.
    pub fn set_stream_options(&mut self, options: crate::types::StreamOptions) {
        self.control.turn_runtime.lock().unwrap().stream_options = options.clone();
        self.agent.set_stream_options(options);
    }

    /// The per-request provider options the next turn applies.
    pub fn stream_options(&self) -> crate::types::StreamOptions {
        self.control
            .turn_runtime
            .lock()
            .unwrap()
            .stream_options
            .clone()
    }

    /// Attempts used by the current auto-retry lifecycle.
    pub fn retry_attempt(&self) -> u32 {
        self.retry_attempt
    }

    /// Observe the auto-retry lifecycle (`auto_retry_start`/`auto_retry_end`).
    pub fn on_auto_retry(&mut self, observer: impl Fn(RetryEvent) + Send + Sync + 'static) {
        self.retry_observer = Some(Arc::new(observer));
    }

    /// Subscribe to harness-level events (queue updates, settled, runtime
    /// mutations). Listeners are sync callbacks fired in registration order
    /// at the moment the state changes.
    /// Delivery stops when the returned subscription is dropped.
    pub fn subscribe_harness(&mut self, listener: HarnessListener) -> HarnessSubscription {
        let id = self
            .control
            .next_harness_listener_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.control
            .harness_listeners
            .lock()
            .unwrap()
            .push((id, listener));
        HarnessSubscription {
            id,
            control: Arc::clone(&self.control),
        }
    }

    /// Fire a harness event to every registered listener.
    fn emit_harness(&self, event: HarnessEvent) {
        self.control.emit_harness(event);
    }

    /// Emit the current queue counts after any enqueue / drain / clear.
    fn emit_queue_update(&self) {
        let next_turn = self.control.next_turn_queue.lock().unwrap().len();
        self.control.emit_queue_counts(
            self.agent.queued_steering_count(),
            self.agent.queued_follow_up_count(),
            next_turn,
        );
    }

    /// Whether [`AgentHarness::request_shutdown`] was called.
    pub fn is_shutdown(&self) -> bool {
        self.control
            .shutdown
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Begin shutdown: stop accepting new operations, cancel the active run
    /// and any retry backoff, clear every queue, and reject further work
    /// with a typed error. Idempotent.
    pub fn request_shutdown(&self) {
        if self
            .control
            .shutdown
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.agent.abort();
        self.control.retry_cancel.lock().unwrap().cancel();
        self.control.operation_cancel.lock().unwrap().cancel();
        let (cleared_steer, cleared_follow_up) = self.agent.clear_queues();
        self.control.emit_harness(HarnessEvent::Abort {
            cleared_steer,
            cleared_follow_up,
        });
        self.control.next_turn_queue.lock().unwrap().clear();
        // Drop unpersisted mutations: after shutdown nothing flushes them, so
        // they must not linger and surface on a later run.
        self.control.pending_mutations.lock().unwrap().clear();
        self.emit_queue_update();
    }

    /// Resolve once the harness is fully settled after a shutdown.
    pub async fn wait_for_shutdown(&self) {
        self.handle().wait_for_idle().await;
    }

    /// The typed error every structured operation returns after shutdown.
    fn ensure_running(&self) -> Result<(), anyhow::Error> {
        if self.is_shutdown() {
            anyhow::bail!("harness is shut down; start a new session to continue");
        }
        Ok(())
    }

    /// Run a skill by name with additional instructions appended to the
    /// skill block — the TS `skill(name, instructions?)`.
    pub async fn skill_with_instructions(
        &mut self,
        name: &str,
        additional_instructions: &str,
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.ensure_running()?;
        let skill = self
            .resources
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown skill: {name}"))?;
        self.prompt(&format_skill_invocation(
            skill,
            Some(additional_instructions),
        ))
        .await
    }

    /// Append a message to the session: immediately when idle, through the
    /// mutation queue when a run is in flight (flushed at the next turn
    /// boundary, like TS `appendMessage`).
    pub async fn append_message(&mut self, message: AgentMessage) -> Result<(), anyhow::Error> {
        self.ensure_running()?;
        if self.phase == AgentHarnessPhase::Idle {
            let entry_id = self.session.append_message(message.clone()).await?;
            // Session-first: a message the next request must carry is only
            // added to the live transcript once it is durable, so a failed
            // append cannot leave the two disagreeing. The entry id joins the
            // index in the same step, keeping it aligned with the transcript.
            self.agent.append_to_transcript(message);
            self.message_entry_ids.push(Some(entry_id));
        } else {
            self.control
                .pending_mutations
                .lock()
                .unwrap()
                .push(PendingMutation::Message(message));
        }
        Ok(())
    }

    /// Replace the mounted tool set, re-applying the active selection. A
    /// duplicate tool name is a configuration bug and is refused.
    pub fn set_tools(
        &mut self,
        tools: Arc<[Arc<dyn crate::tool::AgentTool>]>,
    ) -> Result<(), anyhow::Error> {
        let mut seen = std::collections::HashSet::new();
        for tool in tools.iter() {
            if !seen.insert(tool.name().to_string()) {
                anyhow::bail!("duplicate tool name: {}", tool.name());
            }
        }
        self.all_tools = tools;
        self.apply_active_tools();
        Ok(())
    }

    /// The full mounted tool set.
    pub fn tools(&self) -> Arc<[Arc<dyn crate::tool::AgentTool>]> {
        Arc::clone(&self.all_tools)
    }

    /// Replace the mounted resources, emitting a `ResourcesUpdate`.
    pub fn set_resources(&mut self, resources: HarnessResources) {
        self.resources = resources;
        self.rebuild_system_prompt();
        self.emit_harness(HarnessEvent::ResourcesUpdate);
    }

    /// The steering queue drain mode.
    pub fn steering_mode(&self) -> crate::agent::QueueMode {
        self.agent.steering_mode()
    }

    /// Change the steering queue drain mode.
    pub fn set_steering_mode(&self, mode: crate::agent::QueueMode) {
        self.agent.set_steering_mode(mode);
    }

    /// The follow-up queue drain mode.
    pub fn follow_up_mode(&self) -> crate::agent::QueueMode {
        self.agent.follow_up_mode()
    }

    /// Change the follow-up queue drain mode.
    pub fn set_follow_up_mode(&self, mode: crate::agent::QueueMode) {
        self.agent.set_follow_up_mode(mode);
    }

    /// The active tool subset; `None` when the full mounted set is in play.
    pub fn active_tool_names(&self) -> Option<&[String]> {
        self.active_tool_names.as_deref()
    }

    /// Switch the model the next turn runs against, persisting a
    /// `model_change` entry so a later restore projects the same choice.
    pub async fn set_model(&mut self, model: Model) -> Result<(), anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!(
                "Cannot set model while harness is in {:?} phase",
                self.phase
            );
        }
        // Without a resolver the fixed stream serves one api; a cross-api
        // model change is refused instead of silently talking the wrong
        // protocol.
        if self.stream_resolver.is_none()
            && !self.stream_fn.api().is_empty()
            && model.api != self.stream_fn.api()
        {
            anyhow::bail!(
                "model api \"{}\" is not served by the fixed stream ({}); \
                 plug in a StreamResolver to switch providers",
                model.api,
                self.stream_fn.api()
            );
        }
        self.session
            .append_model_change(&model.provider, &model.id)
            .await?;
        self.control.turn_runtime.lock().unwrap().model = model.clone();
        self.agent.set_model(model.clone());
        self.model = model;
        Ok(())
    }

    /// Narrow the tools the model sees to a subset of the mounted set,
    /// persisting an `active_tools_change` entry so a later restore projects
    /// the same selection.
    pub async fn set_active_tools(
        &mut self,
        active_tool_names: Vec<String>,
    ) -> Result<(), anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!(
                "Cannot set active tools while harness is in {:?} phase",
                self.phase
            );
        }
        for name in &active_tool_names {
            if !self.all_tools.iter().any(|t| t.name() == name) {
                anyhow::bail!("Cannot activate unknown tool: {name}");
            }
        }
        self.session
            .append_active_tools_change(&active_tool_names)
            .await?;
        self.active_tool_names = Some(active_tool_names);
        self.apply_active_tools();
        Ok(())
    }

    /// Set the reasoning tier for following turns, persisting a
    /// `thinking_level_change` entry so a later restore projects it. `None`
    /// reads as `"off"` on the session path. Mid-run changes go through
    /// [`HarnessHandle::set_thinking_level`] instead.
    pub async fn set_thinking_level(
        &mut self,
        thinking_level: Option<String>,
    ) -> Result<(), anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!(
                "Cannot set thinking level while harness is in {:?} phase",
                self.phase
            );
        }
        self.session
            .append_thinking_level_change(thinking_level.as_deref().unwrap_or("off"))
            .await?;
        self.control.turn_runtime.lock().unwrap().thinking_level = thinking_level.clone();
        self.agent.set_thinking_level(thinking_level);
        Ok(())
    }

    /// Sync the harness's own state (agent model/thinking) from the shared
    /// runtime snapshot. Called before every new run and before post-run
    /// maintenance, so continuations and overflow recovery read the same
    /// model the turns just used. The durable session writes are flushed
    /// separately.
    fn apply_turn_runtime(&mut self) {
        let snapshot = self.control.turn_runtime.lock().unwrap().clone();
        let current = self.agent.state().model.clone();
        if snapshot.model != current {
            self.agent.set_model(snapshot.model.clone());
            self.model = snapshot.model;
        }
        if snapshot.thinking_level != self.agent.state().thinking_level {
            self.agent.set_thinking_level(snapshot.thinking_level);
        }
        if let Some(names) = snapshot.active_tool_names
            && self.active_tool_names.as_deref() != Some(names.as_slice())
        {
            self.active_tool_names = Some(names);
            self.apply_active_tools();
        }
        if snapshot.stream_options != *self.agent.stream_options() {
            self.agent
                .set_stream_options(snapshot.stream_options.clone());
        }
    }

    /// Persist runtime mutations queued while a run was in flight, one at a
    /// time: each entry leaves the queue only after its session append
    /// succeeds, so a failed write keeps the tail (and the current entry)
    /// for the next flush instead of dropping it. Runtime state is synced by
    /// [`AgentHarness::apply_turn_runtime`], not here.
    async fn flush_pending_mutations(&mut self) -> Result<(), anyhow::Error> {
        // Adopt whatever the mid-run boundary already made durable: those
        // messages produced no `MessageEnd`, so this is the only path by which
        // they reach the transcript — and the only one that can keep
        // `message_entry_ids` aligned with it.
        let flushed: Vec<(AgentMessage, String)> = self
            .control
            .flushed_messages
            .lock()
            .unwrap()
            .drain(..)
            .collect();
        for (message, entry_id) in flushed {
            self.agent.append_to_transcript(message);
            self.message_entry_ids.push(Some(entry_id));
        }
        let had_pending_mutations = !self.control.pending_mutations.lock().unwrap().is_empty()
            || self
                .control
                .flushed_any_mutation
                .swap(false, std::sync::atomic::Ordering::Relaxed);
        let result = self.flush_pending_mutations_inner().await;
        // Reached whether or not the queue drained cleanly: the save point
        // reports what is durable, and a partial flush still advanced that.
        self.control.emit_harness(HarnessEvent::SavePoint {
            had_pending_mutations,
        });
        result
    }

    /// Drain the mutation queue, popping each entry only after its append
    /// succeeds so a failure leaves the rest queued for the next flush.
    async fn flush_pending_mutations_inner(&mut self) -> Result<(), anyhow::Error> {
        loop {
            let next = self
                .control
                .pending_mutations
                .lock()
                .unwrap()
                .first()
                .cloned();
            let Some(mutation) = next else {
                return Ok(());
            };
            match mutation {
                PendingMutation::Model(model) => {
                    self.session
                        .append_model_change(&model.provider, &model.id)
                        .await?;
                }
                PendingMutation::ThinkingLevel(level) => {
                    self.session
                        .append_thinking_level_change(level.as_deref().unwrap_or("off"))
                        .await?;
                }
                PendingMutation::ActiveTools(names) => {
                    self.session.append_active_tools_change(&names).await?;
                }
                PendingMutation::Message(message) => {
                    let entry_id = self.session.append_message(message.clone()).await?;
                    // Deferred to here rather than to the run that queued it,
                    // so the message lands after that turn's own messages
                    // instead of splitting a tool call from its result.
                    self.agent.append_to_transcript(message);
                    self.message_entry_ids.push(Some(entry_id));
                }
            }
            // The append succeeded: drop the entry and continue.
            self.control.pending_mutations.lock().unwrap().remove(0);
        }
    }

    /// Re-derive the agent's tool list from the mounted set and the active
    /// selection. The full set mounts when no selection ever narrowed it.
    fn apply_active_tools(&mut self) {
        let tools: Vec<Arc<dyn crate::tool::AgentTool>> = match &self.active_tool_names {
            Some(names) => self
                .all_tools
                .iter()
                .filter(|t| names.iter().any(|n| n == t.name()))
                .cloned()
                .collect(),
            None => self.all_tools.to_vec(),
        };
        self.agent.set_tools(tools.into());
        self.rebuild_system_prompt();
    }

    /// Register a hook handler.
    pub fn on(&mut self, hook: HookPoint, handler: HookHandler) {
        self.hooks.lock().unwrap().push((hook, handler));
    }

    /// Run all registered hooks for a given point.
    fn run_hooks(&self, hook: HookPoint, mut ctx: HookContext) -> HookContext {
        let hooks = self.hooks.lock().unwrap();
        for (point, handler) in hooks.iter() {
            if *point == hook {
                ctx = handler(ctx);
            }
        }
        ctx
    }

    /// Send a user prompt and run the agent loop.
    ///
    /// Returns the messages produced during this turn. The messages are
    /// also persisted to the session.
    ///
    /// A turn ending in a transient provider failure is retried in place
    /// after an exponential backoff (`RetrySettings`, default 3 attempts at
    /// 2s doubling); a context-overflow error from the current model is
    /// instead compacted and retried once (`run_overflow_recovery`). Both
    /// drop the failed turn's terminal message from the transcript — it
    /// stays persisted — so the retry runs against the same context that
    /// preceded the failure. A settled turn whose context crossed the
    /// compaction threshold is compacted for the next turn without a retry;
    /// a failed maintenance compaction never turns the settled turn into an
    /// error. Messages queued during the run or any maintenance step are
    /// delivered before the call returns.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.prompt_input(PromptInput::text(text)).await
    }

    /// Send a prompt batch (text plus optional image content) and run the
    /// agent loop. The pre-prompt checks — runtime snapshot sync and the
    /// aborted-turn compaction — apply exactly as for [`AgentHarness::prompt`].
    pub async fn prompt_input(
        &mut self,
        input: PromptInput,
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.ensure_running()?;
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot prompt while harness is in {:?} phase", self.phase);
        }
        let _active = ActiveGuard::arm(&self.control);
        // A handle mutation queued while idle applies to this run's first
        // turn.
        self.apply_turn_runtime();

        // An aborted turn skips the post-run threshold check, so the oversized
        // context would otherwise wait for a real overflow to compact. TS
        // checks again before the next prompt (`skipAbortedCheck: false`); a
        // failed maintenance compaction here never blocks the prompt.
        if matches!(
            self.agent.state().messages.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Aborted),
                ..
            })
        ) && self.needs_compaction()
            && let Err(e) = self.compact(None).await
        {
            tracing::warn!("pre-prompt compaction failed: {e:#}");
        }

        self.phase = AgentHarnessPhase::Turn;
        // A new user message rearms the one-shot overflow recovery.
        self.overflow_recovery_attempted = false;

        // Run before-agent-start hooks. Their result steers the run: injected
        // messages extend the prompt batch after the user message, and a
        // system prompt override reaches the run's initial context only.
        let hook_ctx = self.run_hooks(
            HookPoint::BeforeAgentStart,
            HookContext::new(HookPoint::BeforeAgentStart).with_data(
                serde_json::to_value(BeforeAgentStartEvent {
                    kind: "before_agent_start",
                    prompt: &input.text,
                    system_prompt: &self.agent.state().system_prompt,
                })
                .expect("BeforeAgentStartEvent serializes"),
            ),
        );

        let mut content = Vec::with_capacity(input.images.len() + 1);
        content.push(ContentBlock::Text {
            text: input.text,
            signature: None,
        });
        content.extend(input.images);
        let user_message = AgentMessage::User {
            content,
            timestamp: chrono::Utc::now(),
        };
        let mut batch = Vec::new();
        // pi-agent-core semantics: the harness's own next-turn queue runs
        // BEFORE the prompt's user message (TS agent-harness executeTurn).
        // The coding facade's asides (its pending next-turn messages) follow
        // the user message, then hook-injected messages.
        batch.append(&mut self.control.next_turn_queue.lock().unwrap());
        self.emit_queue_update();
        batch.push(user_message);
        batch.extend(input.asides);
        batch.extend(hook_ctx.inject_messages);

        let prior_system_prompt = self.agent.state().system_prompt.clone();
        if let Some(override_) = &hook_ctx.system_prompt_override {
            self.agent.set_system_prompt(override_.clone());
        }
        let result = self.agent.prompt_messages(&batch).await;
        if hook_ctx.system_prompt_override.is_some() {
            self.agent.set_system_prompt(prior_system_prompt);
        }

        match result {
            Ok(messages) => {
                self.phase = AgentHarnessPhase::Idle;
                self.note_run_outcome(&messages);

                let mut all_messages = messages;
                all_messages.extend(self.settle_after_run().await?);
                // The persistence middleware wrote every MessageEnd at emit
                // time; only the entry-id bookkeeping lands here.
                self.drain_turn_entry_ids();
                self.flush_pending_mutations().await?;
                Ok(all_messages)
            }
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                // A middleware append failure aborted the run: the transcript
                // may hold messages the session never recorded, so revert to
                // the persisted prefix before surfacing the error.
                let revert = self.restore().await;
                let _ = self.flush_pending_mutations().await;
                Err(match revert {
                    Ok(()) => e,
                    Err(re) => anyhow::anyhow!(
                        "{e:#}; reverting the transcript to the persisted session also failed: {re:#}"
                    ),
                })
            }
        }
    }

    /// Continue from the current transcript.
    ///
    /// The same post-run maintenance as [`AgentHarness::prompt`] applies:
    /// transient provider errors are auto-retried after a backoff, an
    /// overflow terminal from the current model is compacted and retried
    /// once per error episode, a settled turn over the compaction threshold
    /// is compacted for the next turn without a retry, and queued messages
    /// are delivered before the call returns.
    pub async fn continue_(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.ensure_running()?;
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot continue while harness is in {:?} phase", self.phase);
        }
        let _active = ActiveGuard::arm(&self.control);
        self.apply_turn_runtime();

        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;

        match result {
            Ok(messages) => {
                self.phase = AgentHarnessPhase::Idle;
                self.note_run_outcome(&messages);

                let mut all_messages = messages;
                all_messages.extend(self.settle_after_run().await?);
                self.drain_turn_entry_ids();
                self.flush_pending_mutations().await?;
                Ok(all_messages)
            }
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                let revert = self.restore().await;
                let _ = self.flush_pending_mutations().await;
                Err(match revert {
                    Ok(()) => e,
                    Err(re) => anyhow::anyhow!(
                        "{e:#}; reverting the transcript to the persisted session also failed: {re:#}"
                    ),
                })
            }
        }
    }

    /// Merge the entry ids the persistence middleware recorded for the
    /// just-run messages into the harness's transcript alignment. The
    /// middleware appended each `MessageEnd` to the session at emit time, so
    /// the durable write is already done; only the id bookkeeping lands here.
    fn drain_turn_entry_ids(&mut self) {
        let ids = std::mem::take(&mut *self.control.message_entry_ids.lock().unwrap());
        self.message_entry_ids.extend(ids);
    }

    /// Rearm the one-shot overflow recovery and close out an in-progress
    /// retry lifecycle when a run produced any non-error assistant message —
    /// both budgets apply per error episode, and a completed assistant reply
    /// ends the episode.
    fn note_run_outcome(&mut self, messages: &[AgentMessage]) {
        let succeeded = messages.iter().any(|m| {
            matches!(
                m,
                AgentMessage::Assistant { stop_reason, .. } if *stop_reason != Some(StopReason::Error)
            )
        });
        if succeeded {
            self.overflow_recovery_attempted = false;
            if self.retry_attempt > 0 {
                let attempt = self.retry_attempt;
                self.retry_attempt = 0;
                self.emit_retry(RetryEvent::End {
                    operation: RetryOperation::Turn,
                    success: true,
                    attempt,
                    final_error: None,
                });
            }
        }
    }

    /// Drive the overflow → compact → retry loop after a finished run.
    ///
    /// Each iteration inspects the transcript's last message; a retryable
    /// overflow runs one compact-and-retry and the loop re-examines the
    /// retry's outcome, so a second overflow (recovery budget spent) or a
    /// clean reply ends the loop. Returns every message the retry turns
    /// produced, oldest first.
    async fn run_overflow_recovery(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let mut produced = Vec::new();
        while let Some(retry_messages) = self.recover_overflow_once().await? {
            self.note_run_outcome(&retry_messages);
            self.drain_turn_entry_ids();
            produced.extend(retry_messages);
        }
        Ok(produced)
    }

    /// Post-run maintenance shared by `prompt` and `continue_`, mirroring the
    /// TS `while (handlePostAgentRun()) continue()` loop: agent-level
    /// auto-retry of retryable provider errors first, then overflow
    /// compact-and-retry, then threshold compaction. Any messages still
    /// queued — from `agent_end` listeners or while a maintenance step ran —
    /// are delivered by one continuation, and that continuation's own
    /// outcome settles through another iteration. A settled turn with no
    /// pending work exits the loop.
    async fn settle_after_run(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let mut produced = Vec::new();
        // Sync the harness model/agent from the runtime snapshot before
        // overflow recovery and threshold compaction, so both read the
        // context window of the model the turns just used.
        self.apply_turn_runtime();
        loop {
            if self.prepare_auto_retry().await? {
                produced.extend(self.run_continuation_turn().await?);
                continue;
            }
            // An in-progress retry lifecycle closes out when the latest
            // failure is not retryable or the budget is spent — TS emits
            // `auto_retry_end` here, before the compaction path.
            if self.retry_attempt > 0
                && let Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    error_message,
                    ..
                }) = self.agent.state().messages.last()
            {
                let attempt = self.retry_attempt;
                self.retry_attempt = 0;
                self.emit_retry(RetryEvent::End {
                    operation: RetryOperation::Turn,
                    success: false,
                    attempt,
                    final_error: error_message.clone(),
                });
            }
            produced.extend(self.run_overflow_recovery().await?);
            produced.extend(self.run_threshold_compaction().await?);
            if !self.agent.has_queued_messages() {
                self.emit_harness(HarnessEvent::Settled {
                    next_turn_count: self.control.next_turn_queue.lock().unwrap().len(),
                });
                return Ok(produced);
            }
            // Deliver messages queued while the run or a maintenance step
            // was in flight. One continuation drains them; its own outcome
            // settles through the next iteration.
            produced.extend(self.run_continuation_turn().await?);
        }
    }

    /// Threshold compaction after a settled run: the turn already completed,
    /// so the conversation is compacted for the next turn and never
    /// retried. A failed maintenance compaction only logs: the settled turn
    /// keeps its result, and the next turn's settle re-attempts the
    /// compaction. Queued-message delivery happens in the outer
    /// [`AgentHarness::settle_after_run`] loop, not here.
    async fn run_threshold_compaction(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        // A user-aborted turn stays exactly where the user stopped it.
        if matches!(
            self.agent.state().messages.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Aborted),
                ..
            })
        ) {
            return Ok(Vec::new());
        }
        if !self.needs_compaction() {
            return Ok(Vec::new());
        }
        match self.compact(None).await {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<compaction::NothingToCompact>().is_some() => {}
            Err(e) => {
                tracing::warn!("threshold compaction failed: {e:#}");
            }
        }
        Ok(Vec::new())
    }

    /// Start the auto-retry of a retryable provider error when the transcript
    /// ends on one and the retry budget is open: increment the attempt
    /// counter, emit `auto_retry_start`, drop the failed turn's terminal
    /// message from the transcript (the session keeps it), and sleep the
    /// exponential backoff. Returns whether the caller should run the retry
    /// turn. An abort during the backoff closes the lifecycle with
    /// `Retry cancelled` and returns false, mirroring TS `_prepareRetry`.
    /// Context overflow is never retried here — the overflow path owns it.
    async fn prepare_auto_retry(&mut self) -> Result<bool, anyhow::Error> {
        let settings = self.retry_settings;
        if !settings.enabled || self.retry_attempt >= settings.max_retries {
            return Ok(false);
        }
        let Some(message) = self.agent.state().messages.last() else {
            return Ok(false);
        };
        if crate::provider::overflow::is_context_overflow(message, self.model.context_window as u64)
            || !retry::is_retryable_assistant_error(message)
        {
            return Ok(false);
        }
        let AgentMessage::Assistant { error_message, .. } = message else {
            return Ok(false);
        };

        self.retry_attempt += 1;
        let delay = std::time::Duration::from_millis(
            settings.base_delay_ms * 2u64.pow(self.retry_attempt - 1),
        );
        // Arm the cancellation token and the Retry phase BEFORE the Start
        // event goes out: a listener reacting to the event with an abort must
        // cancel this backoff, not the previous (already spent) one.
        let cancel = CancellationToken::new();
        *self.control.retry_cancel.lock().unwrap() = cancel.clone();
        self.phase = AgentHarnessPhase::Retry;
        self.emit_retry(RetryEvent::Start {
            operation: RetryOperation::Turn,
            attempt: self.retry_attempt,
            max_attempts: settings.max_retries,
            delay,
            error_message: error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".into()),
        });

        // The failed turn's terminal message must not reach the retry's
        // context; the session keeps it for history.
        let mut messages = self.agent.state().messages.clone();
        messages.pop();
        self.agent.replace_transcript(messages);
        self.message_entry_ids.pop();

        let slept = tokio::select! {
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(delay) => true,
        };
        self.phase = AgentHarnessPhase::Idle;
        if slept {
            self.emit_retry(RetryEvent::AttemptStart {
                operation: RetryOperation::Turn,
                attempt: self.retry_attempt,
            });
        }
        if !slept {
            let attempt = self.retry_attempt;
            self.retry_attempt = 0;
            self.emit_retry(RetryEvent::End {
                operation: RetryOperation::Turn,
                success: false,
                attempt,
                final_error: Some("Retry cancelled".into()),
            });
        }
        Ok(slept)
    }

    /// One continuation turn over the current transcript — the retry turn
    /// after a dropped error message, or a drain of messages queued by
    /// `agent_end` listeners or a maintenance step. Messages persist and
    /// settle like any other turn.
    async fn run_continuation_turn(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;
        self.phase = AgentHarnessPhase::Idle;
        let messages = result?;
        self.note_run_outcome(&messages);
        self.drain_turn_entry_ids();
        Ok(messages)
    }

    fn emit_retry(&self, event: RetryEvent) {
        if let Some(observer) = &self.retry_observer {
            observer(event);
        }
    }

    /// One overflow recovery step over the finished run.
    ///
    /// Returns `None` when the transcript does not end in a recoverable
    /// overflow — including the terminal states TS settles on: a context
    /// compaction cannot shrink further ([`compaction::NothingToCompact`]),
    /// or the recovery budget for this error episode is already spent.
    /// Otherwise drops the failed turn's terminal message from the
    /// transcript (it stays persisted, mirroring TS), compacts, runs one
    /// retry turn, and returns the retry's messages.
    async fn recover_overflow_once(&mut self) -> Result<Option<Vec<AgentMessage>>, anyhow::Error> {
        let Some(will_retry) = self.overflow_will_retry() else {
            return Ok(None);
        };

        if !will_retry {
            // A completed answer that silently exceeded the window: compact
            // for the next turn. The turn itself cannot be retried — the
            // transcript ends on its assistant reply, which `continue_`
            // refuses.
            return match self.compact(None).await {
                Ok(_) => Ok(None),
                Err(e) if e.downcast_ref::<compaction::NothingToCompact>().is_some() => Ok(None),
                Err(e) => Err(e),
            };
        }

        if self.overflow_recovery_attempted {
            return Ok(None);
        }
        self.overflow_recovery_attempted = true;

        // The failed turn's terminal message must not reach the retry's
        // context; the session keeps it for history.
        let mut messages = self.agent.state().messages.clone();
        debug_assert!(
            matches!(messages.last(), Some(AgentMessage::Assistant { .. })),
            "overflow check passed on the last message"
        );
        messages.pop();
        self.agent.replace_transcript(messages);
        self.message_entry_ids.pop();

        match self.compact(None).await {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<compaction::NothingToCompact>().is_some() => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }

        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;
        self.phase = AgentHarnessPhase::Idle;
        Ok(Some(result?))
    }

    /// Whether the transcript's last message is a context overflow from the
    /// current model, and whether the turn may be retried after compaction.
    ///
    /// The guards mirror TS: an aborted message never triggers recovery, an
    /// error attributed to a different model is not this model's overflow,
    /// and a message recorded at or before the latest compaction measured a
    /// context that no longer exists. A completed (`Stop`) answer compacts
    /// but cannot retry.
    fn overflow_will_retry(&self) -> Option<bool> {
        let message = self.agent.state().messages.last()?;
        let AgentMessage::Assistant {
            stop_reason,
            provider,
            model,
            timestamp,
            ..
        } = message
        else {
            return None;
        };
        if *stop_reason == Some(StopReason::Aborted) {
            return None;
        }
        if provider != &self.model.provider || model != &self.model.id {
            return None;
        }
        if self.last_compaction_at.is_some_and(|at| timestamp <= &at) {
            return None;
        }
        if crate::provider::overflow::is_context_overflow(message, self.model.context_window as u64)
        {
            Some(*stop_reason != Some(StopReason::Stop))
        } else {
            None
        }
    }

    /// Abort the current agent run and any in-flight retry backoff.
    pub fn abort(&mut self) {
        self.control.retry_cancel.lock().unwrap().cancel();
        self.agent.abort();
        self.phase = AgentHarnessPhase::Idle;
    }

    /// Reset the agent's transcript and queues.
    pub fn reset(&mut self) {
        self.agent.reset();
        self.message_entry_ids.clear();
        self.phase = AgentHarnessPhase::Idle;
    }

    /// Load the compaction boundary from the persisted session.
    ///
    /// A harness constructed over an existing session must call this before
    /// relying on [`AgentHarness::needs_compaction`]; a fresh session holds
    /// no compaction entries and the boundary stays unset. Only the boundary
    /// is recovered — [`AgentHarness::restore`] rebuilds the transcript too.
    pub async fn recover_boundary(&mut self) -> Result<(), anyhow::Error> {
        self.last_compaction_at = self.session.latest_compaction_timestamp().await?;
        Ok(())
    }

    /// Move the session cursor to an earlier entry, rebuild the transcript
    /// from the new path, and append a branch summary for it — the TS
    /// `navigateTree` with default options (summarization off).
    pub async fn navigate_tree(
        &mut self,
        target_id: &str,
    ) -> Result<NavigateTreeResult, anyhow::Error> {
        self.navigate_tree_with_options(target_id, NavigateTreeOptions::default())
            .await
    }

    /// [`AgentHarness::navigate_tree`] with the TS option surface. The branch
    /// summary is generated with the current model's runtime only when
    /// `summarize` is set and the abandoned branch is non-empty; a plain
    /// navigation moves the cursor and restores the transcript without a
    /// provider call. The harness is in the `BranchSummary` phase for the
    /// operation's duration, mirroring TS.
    pub async fn navigate_tree_with_options(
        &mut self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<NavigateTreeResult, anyhow::Error> {
        self.ensure_running()?;
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot navigate while harness is in {:?} phase", self.phase);
        }
        let _active = ActiveGuard::arm(&self.control);
        // Arm the operation token so `abort` / `request_shutdown` can end a
        // long-running branch summarization; a fresh token per operation.
        *self.control.operation_cancel.lock().unwrap() = CancellationToken::new();
        self.phase = AgentHarnessPhase::BranchSummary;
        let result = self.navigate_tree_inner(target_id, &options).await;
        self.phase = AgentHarnessPhase::Idle;
        result
    }

    async fn navigate_tree_inner(
        &mut self,
        target_id: &str,
        options: &NavigateTreeOptions,
    ) -> Result<NavigateTreeResult, anyhow::Error> {
        let old_leaf = self.session.leaf_id().await?;
        if old_leaf.as_deref() == Some(target_id) {
            return Ok(NavigateTreeResult {
                cancelled: false,
                aborted: false,
                editor_text: None,
                summary_entry_id: None,
            });
        }
        let target_entry = self
            .session
            .storage()
            .get_entry(target_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("entry {target_id} not found"))?;

        // Collect the branch being left behind: the old leaf up to the common
        // ancestor with the target path. The target's own history stays
        // untouched — TS `collectEntriesForBranchSummary`.
        let old_path = self.session.storage().get_path(old_leaf.as_deref()).await?;
        let target_path = self.session.storage().get_path(Some(target_id)).await?;
        let old_ids: std::collections::HashSet<&str> = old_path.iter().map(|e| e.id()).collect();
        let common_ancestor = target_path
            .iter()
            .rev()
            .find(|e| old_ids.contains(e.id()))
            .map(|e| e.id().to_string());
        let abandoned: Vec<SessionTreeEntry> = old_path
            .iter()
            .rev()
            .take_while(|e| common_ancestor.as_deref() != Some(e.id()))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        // The target's message text rides on the result for user/custom
        // targets — TS `contentText(content, "")`.
        let editor_text = match &target_entry {
            SessionTreeEntry::Message {
                message: AgentMessage::User { content, .. },
                ..
            }
            | SessionTreeEntry::CustomMessage { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        };

        // The before-tree hook sees the full TS-shaped preparation and may
        // cancel the navigation or override the summarization instructions
        // and label. The typed event rides in the context data.
        let mut custom_instructions = options.custom_instructions.clone();
        let mut label = options.label.clone();
        let before_event = SessionBeforeTreeEvent {
            kind: "session_before_tree",
            target_id,
            old_leaf_id: old_leaf.as_deref(),
            common_ancestor_id: common_ancestor.as_deref(),
            entries_to_summarize: &abandoned,
            user_wants_summary: options.summarize,
            custom_instructions: custom_instructions.as_deref(),
            replace_instructions: options.replace_instructions,
            label: label.as_deref(),
        };
        let hook_ctx = self.run_hooks(
            HookPoint::SessionBeforeTree,
            HookContext::new(HookPoint::SessionBeforeTree)
                .with_data(serde_json::to_value(&before_event).unwrap_or(serde_json::Value::Null)),
        );
        if hook_ctx.cancel_tree {
            return Ok(NavigateTreeResult {
                cancelled: true,
                aborted: false,
                editor_text: None,
                summary_entry_id: None,
            });
        }
        if let Some(instructions) = hook_ctx.tree_custom_instructions {
            custom_instructions = Some(instructions);
        }
        if let Some(overridden) = hook_ctx.tree_label {
            label = Some(overridden);
        }
        let replace_instructions = hook_ctx
            .tree_replace_instructions
            .unwrap_or(options.replace_instructions);
        let hook_summary = hook_ctx.tree_summary.map(|s| {
            crate::compaction::branch_summarization::BranchSummaryResult {
                summary: Some(s.summary),
                usage: s.usage,
                read_files: s.read_files,
                modified_files: s.modified_files,
                aborted: false,
            }
        });

        // A summary is generated only when asked for and there is an
        // abandoned branch to summarize (TS `options.summarize &&
        // entriesToSummarize.length > 0`). The token budget is the model's
        // context window minus the reserved prompt/response space; the
        // request carries the harness's per-request stream options; transient
        // failures retry under the harness retry policy; the operation token
        // lets abort/shutdown end the run.
        // A hook summary is honored only when the caller requested one
        // (TS), and carries fromHook provenance through persistence.
        let summary = if options.summarize
            && let Some(hook) = hook_summary
        {
            Some((hook, true))
        } else if options.summarize && !abandoned.is_empty() {
            let stream_fn = match &self.stream_resolver {
                Some(resolver) => resolver(&self.model)?,
                None => Arc::clone(&self.stream_fn),
            };
            let token_budget = (self.model.context_window as u64)
                .saturating_sub(self.branch_summary_reserve as u64);
            let signal = self.control.operation_cancel.lock().unwrap().clone();
            let mut stream_options = self
                .control
                .turn_runtime
                .lock()
                .unwrap()
                .stream_options
                .clone();
            // The branch summary caps its output at 2048 tokens (TS, fixed).
            stream_options.max_tokens = Some(2048);
            let retry = self.retry_settings;
            let mut attempt = 0u32;
            let mut started = false;
            let result = loop {
                attempt += 1;
                match crate::compaction::branch_summarization::summarize_branch(
                    &abandoned,
                    &self.model,
                    Arc::clone(&stream_fn),
                    token_budget,
                    custom_instructions.as_deref(),
                    replace_instructions,
                    signal.clone(),
                    &stream_options,
                )
                .await
                {
                    Ok(result) => {
                        // A retried attempt that finally succeeded closes the
                        // lifecycle as a success.
                        if started {
                            self.emit_retry(RetryEvent::End {
                                operation: RetryOperation::BranchSummary,
                                success: true,
                                attempt,
                                final_error: None,
                            });
                        }
                        break Ok(result);
                    }
                    Err(e) => {
                        // Only transient failures retry (TS
                        // `isRetryableAssistantError`): auth, quota/billing,
                        // and invalid requests are deterministic and surface
                        // immediately, with no lifecycle events.
                        let transient = is_transient_error(&e);
                        let delay = std::time::Duration::from_millis(
                            retry.base_delay_ms.saturating_mul(
                                1u64.checked_shl(attempt.saturating_sub(1))
                                    .unwrap_or(u64::MAX),
                            ),
                        );
                        if !retry.enabled || !transient || attempt > retry.max_retries {
                            break Err(e);
                        }
                        self.emit_retry(RetryEvent::Start {
                            operation: RetryOperation::BranchSummary,
                            attempt,
                            max_attempts: retry.max_retries,
                            delay,
                            error_message: e.to_string(),
                        });
                        started = true;
                        let cancelled = tokio::select! {
                            _ = tokio::time::sleep(delay) => false,
                            _ = signal.cancelled() => true,
                        };
                        if !cancelled {
                            self.emit_retry(RetryEvent::AttemptStart {
                                operation: RetryOperation::BranchSummary,
                                attempt,
                            });
                        }
                        if cancelled {
                            // TS: an aborted summarization cancels the
                            // navigation — a result, not an error, and no
                            // cursor move or entry append.
                            if started {
                                self.emit_retry(RetryEvent::End {
                                    operation: RetryOperation::BranchSummary,
                                    success: false,
                                    attempt,
                                    final_error: Some("branch summary cancelled".into()),
                                });
                            }
                            break Ok(
                                crate::compaction::branch_summarization::BranchSummaryResult {
                                    summary: None,
                                    usage: None,
                                    read_files: Vec::new(),
                                    modified_files: Vec::new(),
                                    aborted: true,
                                },
                            );
                        }
                    }
                }
            };
            let result = match result {
                Ok(result) => result,
                Err(e) => {
                    if started {
                        self.emit_retry(RetryEvent::End {
                            operation: RetryOperation::BranchSummary,
                            success: false,
                            attempt,
                            final_error: Some(e.to_string()),
                        });
                    }
                    return Err(e);
                }
            };
            if result.aborted {
                // TS: an aborted summarization cancels the navigation before
                // any cursor move or entry append.
                return Ok(NavigateTreeResult {
                    cancelled: true,
                    aborted: true,
                    editor_text: None,
                    summary_entry_id: None,
                });
            }
            Some((result, false))
        } else {
            None
        };

        // Move the cursor (a user/custom target focuses its parent, mirroring
        // TS), then hang the summary on the new branch when one was produced.
        let new_leaf = match &target_entry {
            SessionTreeEntry::Message {
                message: AgentMessage::User { .. },
                parent_id,
                ..
            }
            | SessionTreeEntry::CustomMessage { parent_id, .. } => parent_id.clone(),
            _ => Some(target_id.to_string()),
        };
        self.session.move_to(new_leaf.as_deref()).await?;
        let summary_entry_id = match &summary {
            Some((summary, from_hook)) => Some(
                self.session
                    .append_branch_summary(
                        new_leaf.as_deref().unwrap_or("root"),
                        summary.summary.as_deref().unwrap_or(""),
                        &summary.read_files,
                        &summary.modified_files,
                        summary.usage.clone(),
                        *from_hook,
                    )
                    .await?,
            ),
            None => None,
        };
        // A label attaches to the summary entry when one exists, otherwise to
        // the target entry — TS `appendLabelChange` on either node.
        if let Some(label) = &label {
            match &summary_entry_id {
                Some(summary_id) => {
                    self.session
                        .append_label(summary_id, Some(label.clone()))
                        .await?;
                }
                None => {
                    self.session
                        .append_label(target_id, Some(label.clone()))
                        .await?;
                }
            }
        }
        self.restore().await?;

        let new_leaf_id = self.session.leaf_id().await?;
        let from_hook = summary.as_ref().map(|(_, from_hook)| *from_hook);
        let tree_event = SessionTreeEvent {
            kind: "session_tree",
            new_leaf_id: new_leaf_id.as_deref(),
            old_leaf_id: old_leaf.as_deref(),
            summary_entry_id: summary_entry_id.as_deref(),
            from_hook,
        };
        let _ = self.run_hooks(
            HookPoint::SessionTree,
            HookContext::new(HookPoint::SessionTree)
                .with_data(serde_json::to_value(&tree_event).unwrap_or(serde_json::Value::Null)),
        );

        Ok(NavigateTreeResult {
            cancelled: false,
            aborted: false,
            editor_text,
            summary_entry_id,
        })
    }

    /// Rebuild the agent transcript from the persisted session.
    ///
    /// Every message-producing entry variant on the active path projects into
    /// the transcript: messages verbatim, custom messages as `Custom`,
    /// branch/compaction summaries as their tagged user-text carriers. The
    /// kept segment behind a compaction boundary is reconstructed by walking
    /// the tree from its `first_kept_entry_id`, never from the boundary
    /// itself. The reasoning tier the path carries is applied to the agent,
    /// the active tool selection narrows the mounted set, and the model the
    /// path carries is applied when a model resolver is plugged in —
    /// resolving it needs the provider registry, which lives at the facade
    /// layer. Restore never appends entries: the session already records
    /// these choices. The compaction boundary used by token estimation is
    /// recovered alongside.
    pub async fn restore(&mut self) -> Result<(), anyhow::Error> {
        let context = self.session.build_session_context().await?;
        self.agent.clear_transcript_state();
        self.agent.replace_transcript(context.messages);
        self.agent.set_thinking_level(context.thinking_level);
        self.active_tool_names = context
            .active_tool_names
            .or_else(|| self.restore_active_tool_default.clone());
        self.apply_active_tools();
        self.rebuild_system_prompt();
        if let (Some(resolver), Some(model_ref)) = (&self.model_resolver, &context.model)
            && let Some(model) = resolver(model_ref)
        {
            self.agent.set_model(model.clone());
            self.model = model;
        }
        // The shared runtime snapshot follows the restored state, so a
        // handle mutation or next-turn refresh never reverts it.
        self.control.turn_runtime.lock().unwrap().model = self.model.clone();
        self.control.turn_runtime.lock().unwrap().thinking_level =
            self.agent.state().thinking_level.clone();
        self.control.turn_runtime.lock().unwrap().active_tool_names =
            self.active_tool_names.clone();
        // Mutations whose durable write failed are still the caller's latest
        // intent: replay them onto the snapshot so the next provider request
        // runs under them instead of reverting to the persisted model while
        // the entry stays queued (TS keeps `this.model` at the new value
        // until the pending write lands).
        {
            let mut snapshot = self.control.turn_runtime.lock().unwrap();
            for mutation in self.control.pending_mutations.lock().unwrap().iter() {
                match mutation {
                    PendingMutation::Model(model) => snapshot.model = model.clone(),
                    PendingMutation::ThinkingLevel(level) => {
                        snapshot.thinking_level = level.clone()
                    }
                    PendingMutation::ActiveTools(names) => {
                        snapshot.active_tool_names = Some(names.clone())
                    }
                    // A queued message has no runtime effect — it flushes to
                    // the session at the next turn boundary like any mutation.
                    PendingMutation::Message(_) => {}
                }
            }
        }
        self.message_entry_ids = context.message_entry_ids;
        // Any ids the middleware recorded before a failure point at messages
        // the restore already projected from the session — drop them so the
        // next run starts clean.
        self.control.message_entry_ids.lock().unwrap().clear();
        self.recover_boundary().await?;
        Ok(())
    }

    /// Check whether compaction is needed based on current context size.
    pub fn needs_compaction(&self) -> bool {
        let tokens = self.estimate_current_tokens();
        compaction::should_compact(
            tokens,
            self.model.context_window as u64,
            &self.compaction_settings,
        )
    }

    /// Estimate the current token usage of the conversation.
    ///
    /// An assistant usage block only anchors the estimate when it was
    /// recorded after the latest compaction; anything older measured the
    /// pre-compaction prefix and the whole transcript falls back to the
    /// character heuristic.
    fn estimate_current_tokens(&self) -> u64 {
        let messages = &self.agent.state().messages;
        let estimate = compaction::estimate_context_tokens(messages);
        let stale_anchor = match (estimate.last_usage_index, self.last_compaction_at) {
            (Some(i), Some(at)) => messages[i].timestamp() <= at,
            _ => false,
        };
        if stale_anchor {
            messages.iter().map(compaction::estimate_tokens).sum()
        } else {
            estimate.tokens
        }
    }

    /// Run compaction on the current conversation.
    ///
    /// Finds the cut point, asks the model (via the harness stream function)
    /// to summarize the compacted prefix, and persists a `Compaction` entry
    /// carrying the real first-kept entry id, the pre-compaction token count,
    /// and the summarization usage. The retained tail is an in-memory flow
    /// value only: the session reconstructs it by walking the tree from the
    /// first-kept entry id. The agent transcript is rewritten to the summary
    /// message plus the retained tail. A
    /// `session_before_compact` hook receives the typed [`CompactionPreparation`]
    /// and the session branch entries, and may cancel or supply a full
    /// [`BeforeCompactOverride`] persisted verbatim. `custom_instructions`
    /// mirrors the TS `compact(customInstructions?)` argument and is surfaced
    /// on the hook event.
    ///
    /// A transcript whose summarizable range is empty — everything fits in the
    /// keep-recent window, or is already folded into the latest boundary's
    /// summary — is refused with [`compaction::NothingToCompact`] before the
    /// phase changes, any hook fires, or the model is called; the session and
    /// transcript stay untouched.
    ///
    /// Only the transcript is rebuilt; the steering and follow-up queues are
    /// user input and survive compaction. A full clear of both transcript
    /// and queues is [`AgentHarness::reset`].
    pub async fn compact(
        &mut self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, anyhow::Error> {
        self.ensure_running()?;
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot compact while harness is in {:?} phase", self.phase);
        }
        let _active = ActiveGuard::arm(&self.control);
        if self.agent.state().messages.is_empty() {
            anyhow::bail!("Cannot compact an empty transcript");
        }

        // The session branch the harness is compacting — the same entries TS
        // exposes as `branchEntries` on the `session_before_compact` event:
        // the full path to the root, across compaction boundaries.
        let branch_entries = self.session.get_branch().await?;

        let messages = self.agent.state().messages.clone();
        let tokens_before = compaction::estimate_context_tokens(&messages).tokens;
        let cut_point = compaction::find_cut_point_split(
            &messages,
            self.compaction_settings.keep_recent_tokens,
        );
        let kept = &messages[cut_point.first_kept_index..];
        let first_kept_entry_id = self
            .message_entry_ids
            .get(cut_point.first_kept_index)
            .cloned()
            .flatten();

        // The preparation doubles as the emptiness guard: an empty
        // summarizable range is refused here — before the phase change, the
        // hook, and the model call — mirroring TS, where `prepareCompaction`
        // returning `undefined` ends the attempt with "Nothing to compact".
        let preparation = match compaction::build_preparation(
            &branch_entries,
            &messages,
            &cut_point,
            first_kept_entry_id.clone(),
            tokens_before,
            &self.compaction_settings,
        ) {
            Some(p) => p,
            None => return Err(compaction::NothingToCompact.into()),
        };

        self.phase = AgentHarnessPhase::Compaction;

        // The hook fires after the cut analysis — mirroring TS, which prepares
        // the compaction then emits the event with `preparation` +
        // `branchEntries` — so the handler decides on the specific content.
        // The typed event carries the full TS `CompactionPreparation` plus the
        // session branch and custom instructions, rather than a trimmed ad-hoc
        // payload.
        let event = SessionBeforeCompactEvent {
            kind: "session_before_compact",
            preparation: &preparation,
            branch_entries: &branch_entries,
            custom_instructions,
        };
        let hook_ctx = self.run_hooks(
            HookPoint::SessionBeforeCompact,
            HookContext::new(HookPoint::SessionBeforeCompact)
                .with_data(serde_json::to_value(&event).unwrap_or(serde_json::Value::Null)),
        );
        if hook_ctx.cancel_compaction {
            self.phase = AgentHarnessPhase::Idle;
            anyhow::bail!("compaction cancelled by before-compact hook");
        }

        // Resolve the compaction result. A hook override supplies a full
        // TS-shaped `CompactResult`, persisted verbatim (`fromHook = true`) —
        // no field falls back to the harness's cut analysis. Otherwise the
        // harness summarizes the prefix itself, consuming the preparation
        // (previous summary folded into the prompt, file ops folded into the
        // summary text and `details`). An empty summary never persists — it
        // would discard the compacted history; the model path bails inside
        // `summarize_via_model`, the hook path is refused here.
        let (
            summary_text,
            first_kept_entry_id,
            tokens_before,
            usage,
            details,
            retained_tail,
            from_hook,
        ) = match hook_ctx.compact_override {
            Some(o) => {
                if o.summary.trim().is_empty() {
                    self.phase = AgentHarnessPhase::Idle;
                    anyhow::bail!("before-compact hook supplied an empty summary");
                }
                let retained_tail = o.retained_tail;
                (
                    o.summary,
                    o.first_kept_entry_id,
                    o.tokens_before,
                    o.usage,
                    o.details,
                    retained_tail,
                    true,
                )
            }
            None => {
                let (summary, usage, details) = self
                    .summarize_via_model(&preparation, custom_instructions)
                    .await?;
                (
                    summary,
                    first_kept_entry_id,
                    tokens_before,
                    usage,
                    details,
                    kept.to_vec(),
                    false,
                )
            }
        };

        let authorship = CompactionAuthorship {
            details: details.clone(),
            from_hook,
        };

        // Persist the boundary first — the session is the durable record and
        // a failure here leaves the agent transcript untouched. The retained
        // tail persists with it, so a restore reads the same messages the
        // transcript is about to be rebuilt from.
        let boundary = match self
            .session
            .append_compaction(
                &summary_text,
                first_kept_entry_id.clone(),
                tokens_before,
                usage.clone(),
                authorship,
                Some(retained_tail.clone()),
            )
            .await
        {
            Ok((_id, timestamp)) => timestamp,
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(e);
            }
        };

        // Rebuild the transcript: summary as context + the retained tail. The
        // summary message carries the boundary instant, so a transcript
        // rebuilt from storage equals this one exactly.
        let mut new_messages = Vec::with_capacity(retained_tail.len() + 1);
        new_messages.push(crate::session::compaction_summary_message(
            &summary_text,
            boundary,
        ));
        new_messages.extend_from_slice(&retained_tail);

        self.agent.clear_transcript_state();
        self.agent.replace_transcript(new_messages);
        // The summary is synthetic (no entry id). A hook-supplied tail carries
        // unknown entry ids (the hook's messages need not be the harness's
        // persisted entries); the harness-computed tail retains the ids it
        // walked from the session, padding with `None` where the transcript
        // has no persisted entries (e.g. rebuilt via `replace_transcript`).
        let tail_len = retained_tail.len();
        let mut new_ids: Vec<Option<String>> = Vec::with_capacity(tail_len + 1);
        new_ids.push(None);
        if from_hook {
            new_ids.extend((0..tail_len).map(|_| None));
        } else {
            let mut known = self
                .message_entry_ids
                .get(cut_point.first_kept_index..)
                .map(|s| s.to_vec())
                .unwrap_or_default();
            known.resize(tail_len, None);
            new_ids.extend(known);
        }
        self.message_entry_ids = new_ids;

        self.last_compaction_at = Some(boundary);
        let tokens_after = self.estimate_current_tokens();

        let result = CompactionResult {
            summary: summary_text,
            first_kept_entry_id,
            tokens_before,
            tokens_after,
            usage,
            details,
            retained_tail,
            is_split_turn: preparation.is_split_turn,
        };

        // Run after-compact hooks.
        let _hook_ctx = self.run_hooks(
            HookPoint::SessionAfterCompact,
            HookContext::new(HookPoint::SessionAfterCompact).with_data(serde_json::json!({
                "tokens_before": tokens_before,
                "tokens_after": tokens_after,
                "cut_point": cut_point.first_kept_index,
                "is_split_turn": cut_point.is_split_turn,
            })),
        );

        self.phase = AgentHarnessPhase::Idle;
        Ok(result)
    }

    /// Summarize the compacted prefix via the harness stream function.
    ///
    /// Consumes the [`CompactionPreparation`]: `previous_summary` is folded
    /// into the summarization prompt, and the computed file lists are appended
    /// to the summary text and returned as `details` — mirroring TS `compact`.
    /// A split turn summarizes the history and the discarded turn prefix
    /// separately and merges them (TS `isSplitTurn`). A terminal
    /// `Error`/`Aborted` stop reason or an empty summary bails before
    /// anything is persisted so the transcript and session stay intact.
    async fn summarize_via_model(
        &mut self,
        preparation: &CompactionPreparation,
        custom_instructions: Option<&str>,
    ) -> Result<(String, Option<Usage>, Option<JsonValue>), anyhow::Error> {
        let (summary_text, usage) = if preparation.is_split_turn {
            let (history_text, history_usage) = if preparation.messages_to_summarize.is_empty() {
                ("No prior history.".to_string(), None)
            } else {
                let prompt = compaction::build_compaction_prompt(
                    &preparation.messages_to_summarize,
                    preparation.previous_summary.as_deref(),
                    custom_instructions,
                );
                self.summarize_prompt(prompt).await?
            };
            let (prefix_text, prefix_usage) = self
                .summarize_prompt(compaction::build_turn_prefix_prompt(
                    &preparation.turn_prefix_messages,
                ))
                .await?;
            (
                format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix_text}"),
                match (history_usage, prefix_usage) {
                    (Some(a), Some(b)) => Some(merge_usage(&a, &b)),
                    (a, b) => a.or(b),
                },
            )
        } else {
            let prompt = compaction::build_compaction_prompt(
                &preparation.messages_to_summarize,
                preparation.previous_summary.as_deref(),
                custom_instructions,
            );
            self.summarize_prompt(prompt).await?
        };
        let (read_files, modified_files) = compaction::compute_file_lists(&preparation.file_ops);
        let block = compaction::format_file_operations(&read_files, &modified_files);
        let summary_text = format!("{summary_text}{block}");
        let details = serde_json::json!({
            "readFiles": read_files,
            "modifiedFiles": modified_files,
        });
        Ok((summary_text, usage, Some(details)))
    }

    /// One summarization model call: streams the prompt with the
    /// summarization system prompt, no cache writes, and returns the summary
    /// text plus reported usage. A terminal error or empty summary bails.
    async fn summarize_prompt(
        &mut self,
        prompt: String,
    ) -> Result<(String, Option<Usage>), anyhow::Error> {
        let summary_context = AgentContext {
            system_prompt: compaction::SUMMARIZATION_SYSTEM_PROMPT.into(),
            messages: vec![AgentMessage::user(prompt)],
            tools: Arc::from(Vec::new()),
            model: self.model.clone(),
            thinking_level: None,
            cache_retention: CacheRetention::None,
            session_id: None,
            metadata: Default::default(),
            stream_options: Default::default(),
        };
        let signal = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel::<crate::types::AgentEvent>(64);
        // Run the summarization stream concurrently with draining its events:
        // the producer would block on the 64-cap channel once it fills, so the
        // receiver must drain while it runs, not after. With a resolver, the
        // summarization call uses the current model's runtime too.
        let stream_fn = match &self.stream_resolver {
            Some(resolver) => resolver(&self.model)?,
            None => Arc::clone(&self.stream_fn),
        };
        let stream_handle =
            tokio::spawn(async move { stream_fn.stream(&summary_context, signal, event_tx).await });
        // The harness does not surface summarization events; just keep the
        // channel empty so the producer never blocks.
        while event_rx.recv().await.is_some() {}
        let summary_response = match stream_handle.await {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(e);
            }
            Err(join_err) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(anyhow::Error::new(join_err));
            }
        };

        let (summary_text, usage) = extract_summary(&summary_response);
        let failed = match &summary_response {
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error | StopReason::Aborted),
                ..
            } => true,
            _ => summary_text.trim().is_empty(),
        };
        if failed {
            self.phase = AgentHarnessPhase::Idle;
            let label = match &summary_response {
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                } => "error",
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Aborted),
                    ..
                } => "aborted",
                _ => "no summary",
            };
            let detail = match &summary_response {
                AgentMessage::Assistant {
                    error_message: Some(msg),
                    ..
                } => format!(": {msg}"),
                _ => String::new(),
            };
            return Err(anyhow::anyhow!("summarization failed ({label}){detail}"));
        }
        Ok((summary_text, usage))
    }

    /// Build a compaction prompt for the current conversation.
    ///
    /// Returns the prompt that should be sent to the LLM to generate a
    /// summary, along with the cut point index. `None` when the summarizable
    /// range is empty — everything fits in the keep-recent window, or only
    /// the leading summary carrier would be cut.
    pub fn build_compaction_prompt(&self) -> Option<(String, usize)> {
        let messages = self.agent.state().messages.clone();
        if messages.is_empty() {
            return None;
        }

        let cut_point =
            compaction::find_cut_point(&messages, self.compaction_settings.keep_recent_tokens);

        // A leading compaction-summary carrier is folded into the
        // summarization as `previous_summary`, never re-summarized — the same
        // exclusion `build_preparation` derives from the session branch.
        let start = usize::from(messages.first().is_some_and(|m| {
            matches!(m, AgentMessage::User { content, .. }
                if matches!(content.first(), Some(ContentBlock::Text { text, .. }) if text.starts_with(crate::session::COMPACTION_SUMMARY_PREFIX)))
        }));
        if cut_point <= start {
            return None; // Nothing to compact.
        }

        let compacted = &messages[start..cut_point];
        let prompt = compaction::build_compaction_prompt(compacted, None, None);
        Some((prompt, cut_point))
    }
}

/// Build the loop-config observation closures that route the harness's
/// registered hooks into the agent loop. Each closure clones the shared hook
/// list so it sees handlers registered via `on()` after the harness is built.
/// A prompt batch: text plus optional image content, so a turn can carry
/// vision input alongside the prompt string.
#[derive(Debug, Clone, Default)]
pub struct PromptInput {
    pub text: String,
    pub images: Vec<ContentBlock>,
    /// Extra user messages appended after the prompt's own message (the
    /// coding facade's pending next-turn asides). The harness's own
    /// next-turn queue still runs before the prompt message.
    pub asides: Vec<AgentMessage>,
}

impl PromptInput {
    pub fn text(text: impl Into<String>) -> Self {
        PromptInput {
            text: text.into(),
            images: Vec::new(),
            asides: Vec::new(),
        }
    }
}

/// Resources a harness exposes to the model: loaded skills and prompt
/// templates. The harness holds their raw text; loading from disk is the
/// consumer's job.
#[derive(Debug, Clone, Default)]
pub struct HarnessResources {
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    /// AGENTS.md / CLAUDE.md instruction files discovered by the resource
    /// loader. They are not skills: the facade folds them into the system
    /// prompt automatically (TS project instructions), so every turn sees
    /// them without an explicit invocation.
    pub context_files: Vec<ContextFile>,
}

/// A project-instruction file (AGENTS.md / CLAUDE.md) from the agentDir or
/// an ancestor directory.
#[derive(Debug, Clone)]
pub struct ContextFile {
    pub name: String,
    pub location: String,
    pub content: String,
}

/// A skill the agent can invoke.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill file path, used in the invocation block so the model can
    /// resolve relative references.
    pub location: String,
    pub content: String,
}

/// The TS `formatSkillInvocation`: a `<skill name location>` block that tells
/// the model where relative references resolve, optionally followed by
/// additional instructions.
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let dir = std::path::Path::new(&skill.location)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name, skill.location, dir, skill.content
    );
    match additional_instructions {
        Some(extra) => format!("{block}\n\n{extra}"),
        None => block,
    }
}

/// Parse an argument string with shell-style single/double quotes — the TS
/// `parseCommandArgs`. Whitespace splits unquoted tokens; quoted sections
/// keep their inner content.
pub fn parse_command_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in args.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        out.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            },
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The TS coding-agent `substituteArgs`: one regex pass over the template
/// string only, so argument and default values containing placeholder
/// patterns are never re-substituted. Supports `$N`, `$@` / `$ARGUMENTS`,
/// `${@:N}` / `${@:N:L}` slices, and `${N:-default}` / `${@:-default}` /
/// `${ARGUMENTS:-default}` defaults. A bare `$0` yields nothing — TS indexes
/// at -1, which reads as undefined.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let re = regex::Regex::new(
        r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)",
    )
    .expect("static placeholder pattern");
    re.replace_all(content, |caps: &regex::Captures| {
        // ${N:-default} / ${@:-default} / ${ARGUMENTS:-default}
        if let Some(target) = caps.get(1) {
            let n: i64 = target.as_str().parse().unwrap_or(1);
            let value = if n < 1 {
                String::new()
            } else if target.as_str() == "@" || target.as_str() == "ARGUMENTS" {
                all.clone()
            } else {
                args.get((n as usize) - 1).cloned().unwrap_or_default()
            };
            return if value.is_empty() {
                caps[2].to_string()
            } else {
                value
            };
        }
        // ${@:N} / ${@:N:L}
        if let Some(start_m) = caps.get(3) {
            let start = start_m
                .as_str()
                .parse::<usize>()
                .unwrap_or(1)
                .saturating_sub(1)
                .min(args.len());
            return match caps.get(4) {
                Some(len_m) => args[start..]
                    .iter()
                    .take(len_m.as_str().parse::<usize>().unwrap_or(0))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
                None => args[start..].join(" "),
            };
        }
        // $ARGUMENTS / $@ / $N
        let simple = caps.get(5).expect("one alternative matched");
        match simple.as_str() {
            "ARGUMENTS" | "@" => all.clone(),
            n => {
                let n: i64 = n.parse().unwrap_or(1);
                if n < 1 {
                    String::new()
                } else {
                    args.get((n as usize) - 1).cloned().unwrap_or_default()
                }
            }
        }
    })
    .into_owned()
}

/// A named prompt template.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub name: String,
    pub content: String,
}

/// The harness's persistence middleware: appends every `MessageEnd` message
/// to the session immediately — before any listener observes it — and
/// records the entry id for the harness's transcript alignment. An append
/// failure aborts the run, keeping the persisted prefix as the truth.
fn build_persistence_middleware<S: SessionStorage + 'static>(
    control: Arc<HarnessControl>,
    session: Arc<Session<S>>,
) -> EventMiddleware {
    Arc::new(move |event: AgentEvent| {
        let session = Arc::clone(&session);
        let control = Arc::clone(&control);
        Box::pin(async move {
            if let AgentEvent::MessageEnd { message } = event {
                let id = session.append_message((*message).clone()).await?;
                control.message_entry_ids.lock().unwrap().push(Some(id));
            }
            Ok(())
        })
    })
}

fn build_loop_hooks<S: SessionStorage + 'static>(
    hooks: Arc<Mutex<Vec<(HookPoint, HookHandler)>>>,
    control: Arc<HarnessControl>,
    session: Arc<Session<S>>,
) -> LoopHooks {
    let provider = Arc::clone(&hooks);
    let before_provider_request: BeforeProviderRequestHook = Arc::new(move |ctx: &AgentContext| {
        let mut hc = HookContext::new(HookPoint::BeforeProviderRequest).with_context(ctx.clone());
        let list = provider.lock().unwrap();
        for (point, handler) in list.iter() {
            if *point == HookPoint::BeforeProviderRequest {
                hc = handler(hc);
            }
        }
        drop(list);
        // A handler that replaced the context wins; otherwise the original
        // context flows through unchanged.
        hc.agent_context.unwrap_or_else(|| ctx.clone())
    });

    let tool = Arc::clone(&hooks);
    let before_tool_call: BeforeToolCallHook =
        Arc::new(move |id: &str, name: &str, args: &JsonValue| {
            let mut hc = HookContext::new(HookPoint::ToolCall).with_data(serde_json::json!({
                "tool_call_id": id,
                "tool_name": name,
                "args": args.clone(),
            }));
            let list = tool.lock().unwrap();
            for (point, handler) in list.iter() {
                if *point == HookPoint::ToolCall {
                    hc = handler(hc);
                }
            }
            drop(list);
            hc.block_reason
        });

    let result = Arc::clone(&hooks);
    let after_tool_call: AfterToolCallHook = Arc::new(move |r: &AgentToolResult| {
        let mut hc = HookContext::new(HookPoint::ToolResult)
            .with_data(serde_json::json!({
                "is_error": r.is_error,
            }))
            .with_tool_result(r.clone());
        let list = result.lock().unwrap();
        for (point, handler) in list.iter() {
            if *point == HookPoint::ToolResult {
                hc = handler(hc);
            }
        }
        drop(list);
        hc.tool_result.unwrap_or_else(|| r.clone())
    });

    let runtime = Arc::clone(&control);
    let durable = Arc::clone(&session);
    let prepare_next_turn: PrepareTurnHook = Arc::new(move || {
        // Before the next provider request, flush mutations listeners queued
        // at TurnEnd to the session — each entry leaves the queue only after
        // its append succeeds, and a failed write aborts the run so the next
        // request never starts with the new model's messages ahead of its
        // model_change entry (TS `flushPendingSessionWrites`).
        let runtime = Arc::clone(&runtime);
        let durable = Arc::clone(&durable);
        Box::pin(async move {
            let mut appended_messages = Vec::new();
            loop {
                let next = runtime.pending_mutations.lock().unwrap().first().cloned();
                let Some(mutation) = next else { break };
                match &mutation {
                    PendingMutation::Model(model) => {
                        durable
                            .append_model_change(&model.provider, &model.id)
                            .await?;
                    }
                    PendingMutation::ThinkingLevel(level) => {
                        durable
                            .append_thinking_level_change(level.as_deref().unwrap_or("off"))
                            .await?;
                    }
                    PendingMutation::ActiveTools(names) => {
                        durable.append_active_tools_change(names).await?;
                    }
                    PendingMutation::Message(message) => {
                        let entry_id = durable.append_message(message.clone()).await?;
                        // Recorded for both consumers of this boundary: the
                        // loop's in-flight context, and the agent transcript
                        // the harness syncs once the run settles. The entry id
                        // rides along so the transcript sync can keep the
                        // entry-id index aligned — a compaction cutting at one
                        // of these positions needs a real anchor.
                        runtime
                            .flushed_messages
                            .lock()
                            .unwrap()
                            .push((message.clone(), entry_id));
                        appended_messages.push(message.clone());
                    }
                }
                runtime.pending_mutations.lock().unwrap().remove(0);
                // The save point at the end of the run reports the whole
                // boundary's work, including what this mid-turn flush drained.
                runtime
                    .flushed_any_mutation
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let snapshot = runtime.turn_runtime.lock().unwrap();
            let update = crate::types::TurnUpdate {
                model: snapshot.model.clone(),
                thinking_level: snapshot.thinking_level.clone(),
                active_tool_names: snapshot.active_tool_names.clone(),
                appended_messages,
            };
            Ok(Some(update))
        })
    });

    LoopHooks {
        before_provider_request: Some(before_provider_request),
        before_tool_call: Some(before_tool_call),
        after_tool_call: Some(after_tool_call),
        prepare_next_turn: Some(prepare_next_turn),
        should_stop_after_turn: None,
    }
}

/// Pull the summary text and token usage out of the summarization response.
///
/// Only a completed assistant turn carries trustworthy usage; an unfinished
/// or non-assistant response contributes no usage anchor.
/// Merge two summarization usages (history + split-turn prefix) into one.
fn merge_usage(a: &Usage, b: &Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        cache_read_input_tokens: a.cache_read_input_tokens + b.cache_read_input_tokens,
        cache_creation_input_tokens: a.cache_creation_input_tokens + b.cache_creation_input_tokens,
        cache_write_1h: match (a.cache_write_1h, b.cache_write_1h) {
            (Some(x), Some(y)) => Some(x + y),
            (x, y) => x.or(y),
        },
        reasoning_tokens: match (a.reasoning_tokens, b.reasoning_tokens) {
            (Some(x), Some(y)) => Some(x + y),
            (x, y) => x.or(y),
        },
        total_tokens: a.total_tokens + b.total_tokens,
        cost: match (a.cost.as_ref(), b.cost.as_ref()) {
            (Some(x), Some(y)) => Some(crate::types::Cost {
                input: x.input + y.input,
                output: x.output + y.output,
                cache_read: x.cache_read + y.cache_read,
                cache_write: x.cache_write + y.cache_write,
                total: x.total + y.total,
            }),
            (x, y) => x.cloned().or_else(|| y.cloned()),
        },
    }
}

fn extract_summary(message: &AgentMessage) -> (String, Option<Usage>) {
    match message {
        AgentMessage::Assistant {
            content,
            usage,
            stop_reason: Some(_),
            ..
        } => {
            let text = content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            (text, Some((**usage).clone()))
        }
        _ => (String::new(), None),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::session::SessionStorage;
    use crate::session::SessionTreeEntry;
    use crate::types::{AgentEvent, ContentBlock, StopReason, ThinkingKind, Usage};
    use tokio_util::sync::CancellationToken;

    struct TestStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for TestStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Test response".into(),
                    signature: None,
                }],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Stop),
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    /// A stream whose summarization response is an `Aborted` terminal — the
    /// shape a cancelled or failed compaction call produces.
    struct AbortedSummaryStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for AbortedSummaryStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            Ok(AgentMessage::Assistant {
                content: vec![],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Aborted),
                usage: Box::new(Usage::default()),
                error_message: Some("summarization was cancelled".into()),
                timestamp: chrono::Utc::now(),
            })
        }
    }

    /// A stream that emits more events than the channel capacity before
    /// returning, exposing a deadlock if the harness drains only after the
    /// producer returns.
    struct ChattyStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for ChattyStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            for i in 0..200u32 {
                let _ = event_tx
                    .send(AgentEvent::MessageUpdate {
                        message: Box::new(AgentMessage::user(format!("chunk {i}"))),
                        assistant_message_event: crate::types::AssistantMessageEvent::TextDelta {
                            content_index: 0,
                            delta: format!("chunk {i}"),
                        },
                    })
                    .await;
            }
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "summary".into(),
                    signature: None,
                }],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Stop),
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    fn test_model() -> Model {
        Model {
            provider: "test".into(),
            id: "test".into(),
            api: "test".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    /// The model a session `model_change` resolves to in tests, distinct
    /// from [`test_model`] so a restore's model application is observable.
    fn resolved_model() -> Model {
        Model {
            provider: "anthropic".into(),
            id: "claude-opus".into(),
            api: "test".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    /// A resolver for the `anthropic/claude-opus` reference the test
    /// sessions carry.
    fn test_model_resolver()
    -> impl Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync + 'static {
        |mref: &crate::session::SessionModelRef| {
            (mref.provider == "anthropic" && mref.model_id == "claude-opus").then(resolved_model)
        }
    }

    /// A transcript `find_cut_point` splits mid-way under
    /// [`compact_test_settings`]: four messages of ~100 estimated tokens
    /// each, so the 150-token keep-recent budget retains only the trailing
    /// assistant and leaves a three-message prefix to summarize. The
    /// assistants' 90_000-token usage anchors `needs_compaction` against the
    /// 100_000-token test model.
    fn compactable_transcript() -> Vec<AgentMessage> {
        let long = "x".repeat(400);
        let assistant = |text: String| AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text,
                signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        vec![
            AgentMessage::user(long.clone()),
            assistant(long.clone()),
            AgentMessage::user(long.clone()),
            assistant(long),
        ]
    }

    /// The keep-recent budget that cuts [`compactable_transcript`] after the
    /// second user message.
    fn compact_test_settings() -> CompactionSettings {
        CompactionSettings {
            keep_recent_tokens: 150,
            ..Default::default()
        }
    }

    // In-memory session storage for testing.
    pub(crate) struct MemStorage {
        entries: std::sync::Mutex<Vec<SessionTreeEntry>>,
        leaf_id: std::sync::Mutex<Option<String>>,
        /// Number of `append_entry` calls so far.
        append_calls: std::sync::Mutex<u64>,
        /// Call number at which `append_entry` fails; `u64::MAX` means never.
        fail_at_call: std::sync::Mutex<u64>,
        /// When set, a `model_change` append for this model id fails — the
        /// durability hook for flush tests.
        fail_model_id: std::sync::Mutex<Option<String>>,
    }

    impl MemStorage {
        pub(crate) fn new() -> Self {
            MemStorage {
                entries: std::sync::Mutex::new(Vec::new()),
                leaf_id: std::sync::Mutex::new(None),
                append_calls: std::sync::Mutex::new(0),
                fail_at_call: std::sync::Mutex::new(u64::MAX),
                fail_model_id: std::sync::Mutex::new(None),
            }
        }

        /// Rebuild a storage over persisted entries, cursor at the tail — the
        /// same state a reopen of the JSONL file would produce.
        fn from_entries(entries: Vec<SessionTreeEntry>) -> Self {
            let leaf_id = entries.last().and_then(SessionTreeEntry::leaf_cursor_after);
            MemStorage {
                entries: std::sync::Mutex::new(entries),
                leaf_id: std::sync::Mutex::new(leaf_id),
                append_calls: std::sync::Mutex::new(0),
                fail_at_call: std::sync::Mutex::new(u64::MAX),
                fail_model_id: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionStorage for MemStorage {
        async fn create_entry_id(&self) -> Result<String, anyhow::Error> {
            Ok(uuid::Uuid::new_v4().to_string())
        }
        async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
            if let SessionTreeEntry::ModelChange { model_id, .. } = entry
                && let Some(fail_id) = self.fail_model_id.lock().unwrap().as_ref()
                && fail_id == model_id
            {
                anyhow::bail!("injected model change failure");
            }
            let mut calls = self.append_calls.lock().unwrap();
            *calls += 1;
            if *calls == *self.fail_at_call.lock().unwrap() {
                anyhow::bail!("injected append failure");
            }
            drop(calls);
            // Advance the cursor as part of the append, mirroring the JSONL
            // backend's contract: a `get_leaf_id` right after reflects this
            // entry without a separate `set_leaf_id`.
            let cursor = entry.leaf_cursor_after();
            self.entries.lock().unwrap().push(entry.clone());
            *self.leaf_id.lock().unwrap() = cursor;
            Ok(())
        }
        async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.id() == id)
                .cloned())
        }
        async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error> {
            Ok(self.leaf_id.lock().unwrap().clone())
        }
        async fn set_leaf_id(&self, leaf_id: Option<&str>) -> Result<(), anyhow::Error> {
            *self.leaf_id.lock().unwrap() = leaf_id.map(|s| s.to_string());
            Ok(())
        }
        async fn get_entries(
            &self,
            cursor: crate::session::SessionEntryCursor,
        ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
            let entries = self.entries.lock().unwrap();
            let tail = entries.iter().skip(cursor.after_entry_seq);
            Ok(match cursor.limit {
                Some(limit) => tail.take(limit).cloned().collect(),
                None => tail.cloned().collect(),
            })
        }
        async fn find_entries(
            &self,
            entry_type: crate::session::EntryType,
        ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .filter(|e| crate::session::entry_kind(e) == entry_type)
                .cloned()
                .collect())
        }
        async fn get_label(&self, id: &str) -> Result<Option<String>, anyhow::Error> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    SessionTreeEntry::Label {
                        target_id, label, ..
                    } if target_id == id => Some(label.as_deref().unwrap_or("").trim().to_string()),
                    _ => None,
                })
                .next_back()
                .filter(|l| !l.is_empty()))
        }
        async fn get_session_name(&self) -> Result<Option<String>, anyhow::Error> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    SessionTreeEntry::SessionInfo { name, .. } => {
                        Some(name.as_deref().unwrap_or("").trim().to_string())
                    }
                    _ => None,
                })
                .next_back()
                .filter(|n| !n.is_empty()))
        }
        async fn get_session_stats(&self) -> Result<crate::session::SessionStats, anyhow::Error> {
            let entries = self.entries.lock().unwrap();
            let mut stats = crate::session::SessionStats::default();
            for entry in entries.iter() {
                let usage = match entry {
                    SessionTreeEntry::Message { message, .. } => {
                        stats.message_count += 1;
                        match message {
                            AgentMessage::Assistant { usage, .. } => Some(&**usage),
                            _ => None,
                        }
                    }
                    SessionTreeEntry::Compaction { usage, .. }
                    | SessionTreeEntry::BranchSummary { usage, .. } => usage.as_ref(),
                    _ => None,
                };
                let Some(usage) = usage.filter(|u| u.cost.is_some()) else {
                    continue;
                };
                let cost = usage.cost.as_ref().expect("filtered on cost presence");
                stats.cached_tokens += usage.cache_read_input_tokens;
                stats.uncached_tokens += usage.input_tokens + usage.cache_creation_input_tokens;
                stats.total_tokens += usage.input_tokens
                    + usage.output_tokens
                    + usage.cache_read_input_tokens
                    + usage.cache_creation_input_tokens;
                stats.cost_total += cost.total;
            }
            Ok(stats)
        }
        async fn get_path(
            &self,
            leaf_id: Option<&str>,
        ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
            // Mirror the JSONL backend: walk the full path to the root, with
            // the same loud errors for an unknown leaf or a broken chain.
            // Returning every entry regardless of the cursor would mask
            // path-relative logic (e.g. `previousSummary` extraction).
            let entries = self.entries.lock().unwrap();
            let target_id = match leaf_id {
                None => return Ok(Vec::new()),
                Some(id) if entries.iter().any(|e| e.id() == id) => id.to_string(),
                Some(id) => anyhow::bail!("entry {id} not found"),
            };
            let mut index: std::collections::HashMap<&str, &SessionTreeEntry> =
                entries.iter().map(|e| (e.id(), e)).collect();
            let mut path: Vec<&SessionTreeEntry> = Vec::new();
            let mut current_id: Option<&str> = Some(&target_id);
            while let Some(id) = current_id {
                let entry = match index.remove(id) {
                    Some(e) => e,
                    None => anyhow::bail!("entry {id} not found: session chain is broken"),
                };
                current_id = entry.parent_id();
                path.push(entry);
            }
            path.reverse();
            Ok(path.into_iter().cloned().collect())
        }
    }

    #[tokio::test]
    async fn test_harness_prompt() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);

        let result = harness.prompt("Hello").await;
        assert!(result.is_ok());

        let messages = result.unwrap();
        assert!(!messages.is_empty());
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    #[tokio::test]
    async fn test_harness_phase_guard() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        // Directly set phase to Turn to test the guard.
        harness.phase = AgentHarnessPhase::Turn;
        let result = harness.prompt("Hello").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_harness_abort() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness.abort();
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    #[tokio::test]
    async fn test_harness_needs_compaction() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        // With empty messages, should not need compaction.
        assert!(!harness.needs_compaction());
    }

    #[tokio::test]
    async fn test_compact_drops_stale_usage_anchor() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        assert!(harness.needs_compaction());

        // After compaction the retained usage measured a different prefix, so
        // the post-compaction count is the plain character heuristic and the
        // harness does not immediately ask for another compaction.
        let result = harness.compact(None).await.unwrap();
        assert!(
            result.tokens_after < 1_000,
            "tokens_after={}",
            result.tokens_after
        );
        assert!(!harness.needs_compaction());
    }

    /// Compaction appends exactly one entry — the `compaction` itself — and
    /// the cursor advances to it via that single append. No companion `leaf`
    /// entry is written, so a failed `append_entry` cannot leave a compacted
    /// transcript with no boundary.
    #[tokio::test]
    async fn test_compact_appends_single_entry_and_advances_cursor() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        assert!(harness.needs_compaction());

        let before = *harness.session().storage().append_calls.lock().unwrap();
        harness.compact(None).await.unwrap();
        let after = *harness.session().storage().append_calls.lock().unwrap();

        // One append_entry for the compaction; no spurious leaf write.
        assert_eq!(after - before, 1);

        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let compaction_id = match entries.last() {
            Some(SessionTreeEntry::Compaction { id, .. }) => id.clone(),
            other => panic!("expected a compaction entry, got {:?}", other),
        };
        // The cursor is the compaction's own id, not a leaf's target.
        let leaf = harness
            .session()
            .storage()
            .get_leaf_id()
            .await
            .unwrap()
            .expect("cursor must advance to the compaction");
        assert_eq!(leaf, compaction_id);
    }

    /// Compaction swaps the transcript but never the queues: steering and
    /// follow-up messages pending at compaction time stay deliverable, where
    /// a full reset would silently drop them. TS pairs this with one
    /// continuation after auto-compaction so the surviving queue drains.
    #[tokio::test]
    async fn test_compact_preserves_queued_messages() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());

        harness.agent_mut().steer(AgentMessage::user("steered in"));
        harness
            .agent_mut()
            .follow_up(AgentMessage::user("followed up"));
        assert!(harness.agent().has_queued_messages());

        harness.compact(None).await.unwrap();
        assert!(
            harness.agent().has_queued_messages(),
            "compaction must not drop queued user input"
        );

        // Both queues drain into the next run, steering before follow-up.
        harness.continue_().await.unwrap();
        assert!(!harness.agent().has_queued_messages());
        let user_texts: Vec<&str> = harness
            .agent()
            .state()
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::User { content, .. } => match &content[0] {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let steered = user_texts
            .iter()
            .position(|t| *t == "steered in")
            .expect("steering message delivered");
        let followed = user_texts
            .iter()
            .position(|t| *t == "followed up")
            .expect("follow-up message delivered");
        assert!(steered < followed);
    }

    /// A transcript that fits inside the keep-recent window has nothing to
    /// summarize: compact refuses with [`compaction::NothingToCompact`]
    /// before the phase changes, any hook fires, or the model is called, and
    /// nothing persists. Mirrors TS `prepareCompaction` returning `undefined`.
    #[tokio::test]
    async fn test_compact_refuses_when_nothing_to_summarize() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingStreamFn {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl StreamFn for CountingStreamFn {
            async fn stream(
                &self,
                _context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "MUST NOT BE CALLED".into(),
                        signature: None,
                    }],
                    model: "test".into(),
                    provider: "test".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Stop),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            }
        }

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CountingStreamFn {
                calls: Arc::clone(&calls),
            }),
        );
        // Two short messages fit the default keep-recent window whole.
        harness.prompt("hello").await.unwrap();
        // The prompt turn consumed one stream call; only compaction's own
        // summarization would add another.
        calls.store(0, Ordering::SeqCst);

        let hook_calls = Arc::new(AtomicUsize::new(0));
        harness.on(HookPoint::SessionBeforeCompact, {
            let hook_calls = Arc::clone(&hook_calls);
            Arc::new(move |ctx: HookContext| {
                hook_calls.fetch_add(1, Ordering::SeqCst);
                ctx
            })
        });

        let err = harness.compact(None).await.unwrap_err();
        assert!(
            err.downcast_ref::<compaction::NothingToCompact>().is_some(),
            "typed refusal, got: {err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 0, "no hook fired");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
        assert!(
            harness
                .session()
                .storage()
                .get_entries(Default::default())
                .await
                .unwrap()
                .iter()
                .all(|e| !matches!(e, SessionTreeEntry::Compaction { .. })),
            "no compaction entry persisted"
        );
        // The transcript is untouched.
        assert_eq!(harness.agent().state().messages.len(), 2);
    }

    /// After a compaction, a transcript of summary carrier + retained tail
    /// that still fits the keep-recent window is refused too: the carrier is
    /// folded in as `previous_summary`, never re-summarized, so the
    /// summarizable range is empty.
    #[tokio::test]
    async fn test_compact_refuses_when_only_summary_carrier_would_be_cut() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        harness.compact(None).await.unwrap();

        // The post-compaction transcript (summary + one retained assistant)
        // fits the window: a second compact finds nothing new to summarize.
        let err = harness.compact(None).await.unwrap_err();
        assert!(
            err.downcast_ref::<compaction::NothingToCompact>().is_some(),
            "typed refusal, got: {err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
        assert_eq!(harness.agent().state().messages.len(), 2);
    }

    /// A stream fn playing a scripted sequence of conversation outcomes.
    /// The summarization call — recognized by its system prompt — bypasses
    /// the script and always succeeds with a fixed summary.
    struct ScriptedStreamFn {
        script: std::sync::Mutex<std::collections::VecDeque<ScriptedTurn>>,
        summaries: Arc<std::sync::atomic::AtomicUsize>,
        fail_summaries: bool,
        /// Runs inside the summarization call, e.g. to queue a follow-up
        /// while compaction is in flight.
        on_summary: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    enum ScriptedTurn {
        /// A completed assistant reply whose text is `0`-repeated `x` bytes.
        Answer(usize),
        /// A completed one-token reply reporting `total_tokens` of context
        /// usage, anchoring the token estimate above the threshold.
        AnswerWithUsage { total_tokens: u64 },
        /// A provider failure classified as context overflow.
        OverflowError,
        /// A transient provider failure the agent-level auto-retry restarts.
        RetryableError,
        /// A completed reply whose reported input exceeded the window — the
        /// silent overflow that compacts without retry.
        SilentOverflow,
        /// An overflow terminal stamped with a different model's identity.
        ForeignModelOverflow,
    }

    impl ScriptedStreamFn {
        fn new(script: Vec<ScriptedTurn>) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
            let summaries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                ScriptedStreamFn {
                    script: std::sync::Mutex::new(script.into()),
                    summaries: Arc::clone(&summaries),
                    fail_summaries: false,
                    on_summary: None,
                },
                summaries,
            )
        }

        fn failing_summaries(mut self) -> Self {
            self.fail_summaries = true;
            self
        }

        fn on_summary(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
            self.on_summary = Some(Arc::new(f));
            self
        }
    }

    fn scripted_assistant(text: String, provider: &str, model: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text,
                signature: None,
            }],
            model: model.into(),
            provider: provider.into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[async_trait::async_trait]
    impl StreamFn for ScriptedStreamFn {
        async fn stream(
            &self,
            context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            if context.system_prompt == compaction::SUMMARIZATION_SYSTEM_PROMPT {
                self.summaries
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(on_summary) = &self.on_summary {
                    on_summary();
                }
                if self.fail_summaries {
                    return Err(anyhow::anyhow!("summarization boom"));
                }
                return Ok(scripted_assistant("summary".into(), "test", "test"));
            }
            match self.script.lock().unwrap().pop_front() {
                Some(ScriptedTurn::Answer(len)) => {
                    Ok(scripted_assistant("x".repeat(len), "test", "test"))
                }
                Some(ScriptedTurn::AnswerWithUsage { total_tokens }) => {
                    let mut message = scripted_assistant("x".into(), "test", "test");
                    if let AgentMessage::Assistant { usage, .. } = &mut message {
                        usage.total_tokens = total_tokens;
                    }
                    Ok(message)
                }
                Some(ScriptedTurn::OverflowError) => Err(anyhow::anyhow!(
                    "http 400: prompt is too long: 213462 tokens > 200000 maximum"
                )),
                Some(ScriptedTurn::RetryableError) => {
                    Err(anyhow::anyhow!("http 529: overloaded, please retry later"))
                }
                Some(ScriptedTurn::SilentOverflow) => {
                    let mut message = scripted_assistant("x".into(), "test", "test");
                    if let AgentMessage::Assistant { usage, .. } = &mut message {
                        usage.input_tokens = 150_000;
                    }
                    Ok(message)
                }
                Some(ScriptedTurn::ForeignModelOverflow) => Ok(AgentMessage::Assistant {
                    content: Vec::new(),
                    model: "other".into(),
                    provider: "other".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Error),
                    usage: Box::new(Usage::default()),
                    error_message: Some(
                        "prompt is too long: 213462 tokens > 200000 maximum".into(),
                    ),
                    timestamp: chrono::Utc::now(),
                }),
                None => panic!("script exhausted"),
            }
        }
    }

    async fn compaction_entries(harness: &AgentHarness<MemStorage>) -> Vec<SessionTreeEntry> {
        harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .expect("entries")
            .into_iter()
            .filter(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
            .collect()
    }

    /// The core closed loop: a turn ending in a context overflow drops its
    /// terminal message from the transcript, compacts, and retries once —
    /// the session keeps the failed turn for history.
    #[tokio::test]
    async fn test_overflow_compacts_and_retries() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // The failed turn's output plus the retry's reply are returned
        // together: user, overflow terminal, recovered assistant.
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(matches!(
            &messages[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                ..
            }
        ));
        assert!(matches!(
            &messages[2],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            }
        ));

        // The transcript holds no error: summary carrier, retained prompt,
        // recovered reply.
        let transcript = &harness.agent().state().messages;
        assert_eq!(transcript.len(), 3, "{transcript:?}");
        assert!(
            transcript.iter().all(|m| !matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            )),
            "the failed terminal left the transcript: {transcript:?}"
        );

        // The session keeps the failed turn and exactly one boundary.
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::Message {
                    message: AgentMessage::Assistant {
                        stop_reason: Some(StopReason::Error),
                        ..
                    },
                    ..
                }
            )),
            "the overflow error stays persisted: {entries:?}"
        );
        assert_eq!(compaction_entries(&harness).await.len(), 1);
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The recovery budget is one-shot per error episode: when the retry
    /// overflows again, the second error surfaces instead of another
    /// compact-and-retry.
    #[tokio::test]
    async fn test_overflow_recovery_is_one_shot() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::OverflowError,
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // Both overflow terminals are returned; no third conversation call
        // happened (the script held exactly two overflow entries).
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "{messages:?}"
        );
        assert_eq!(compaction_entries(&harness).await.len(), 1);
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            matches!(
                harness.agent().state().messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "the second overflow error stands"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A successful retry rearms the recovery budget: a later overflow gets
    /// its own compact-and-retry.
    #[tokio::test]
    async fn test_overflow_recovery_rearms_after_success() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        harness.prompt("second").await.unwrap();
        let messages = harness.prompt("third").await.unwrap();
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Stop),
                    ..
                })
            ),
            "the second episode recovered too: {messages:?}"
        );
        assert_eq!(compaction_entries(&harness).await.len(), 2);
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            harness.agent().state().messages.iter().all(|m| !matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            )),
            "no error survives in the transcript"
        );
    }

    /// An overflow with nothing summarizable cannot shrink the context: the
    /// failed terminal leaves the transcript, no boundary is persisted, and
    /// no retry runs.
    #[tokio::test]
    async fn test_overflow_without_summarizable_range_surfaces_error() {
        let (stream_fn, summaries) =
            ScriptedStreamFn::new(vec![ScriptedTurn::Answer(8), ScriptedTurn::OverflowError]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "{messages:?}"
        );
        assert!(compaction_entries(&harness).await.is_empty());
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The failed terminal left the transcript (the session keeps it);
        // the transcript ends on the second prompt.
        let transcript = &harness.agent().state().messages;
        assert!(matches!(transcript.last(), Some(AgentMessage::User { .. })));
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// An overflow terminal attributed to a different model is not this
    /// model's error to recover: no compaction, no retry.
    #[tokio::test]
    async fn test_overflow_from_other_model_is_not_recovered() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::ForeignModelOverflow,
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });
        let messages = harness.prompt("second").await.unwrap();
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "{messages:?}"
        );
        assert!(compaction_entries(&harness).await.is_empty());
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The foreign error message stays in the transcript.
        assert!(
            matches!(
                harness.agent().state().messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "the untouched error stands"
        );
    }

    /// A settled turn whose reported usage crosses the threshold is
    /// compacted for the next turn — without a retry: the returned messages
    /// are the turn's own, and the transcript is rebuilt behind one
    /// compaction boundary.
    #[tokio::test]
    async fn test_threshold_compaction_fires_after_settled_turn() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::AnswerWithUsage {
                total_tokens: 90_000,
            },
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 0);
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // No retry: exactly the turn's own user message and reply.
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(matches!(
            messages.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            })
        ));
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(compaction_entries(&harness).await.len(), 1);
        // The transcript was rebuilt: summary carrier first, the 90k-usage
        // reply retained in the tail.
        let transcript = &harness.agent().state().messages;
        assert!(transcript.len() < 4, "{transcript:?}");
        assert!(matches!(
            transcript.last(),
            Some(AgentMessage::Assistant { .. })
        ));
    }

    /// A failed maintenance compaction leaves the settled turn's result
    /// intact: prompt resolves with the turn's messages, nothing is
    /// persisted, and the transcript keeps the full conversation.
    #[tokio::test]
    async fn test_threshold_compaction_failure_keeps_turn_result() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::AnswerWithUsage {
                total_tokens: 90_000,
            },
        ]);
        let stream_fn = stream_fn.failing_summaries();
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(compaction_entries(&harness).await.is_empty());
        assert_eq!(harness.agent().state().messages.len(), 4);
    }

    /// A follow-up queued while threshold compaction runs is delivered by
    /// one continuation immediately after — compaction never waits for the
    /// next explicit prompt to drain the queue.
    #[tokio::test]
    async fn test_threshold_compaction_delivers_queued_follow_up() {
        let handle_slot: Arc<std::sync::Mutex<Option<crate::agent::RunHandle>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_in_summary = Arc::clone(&handle_slot);
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::AnswerWithUsage {
                total_tokens: 90_000,
            },
            ScriptedTurn::Answer(8),
        ]);
        let stream_fn = stream_fn.on_summary(move || {
            if let Some(handle) = slot_in_summary.lock().unwrap().as_ref() {
                handle.follow_up(AgentMessage::user("while compacting"));
            }
        });
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        *handle_slot.lock().unwrap() = Some(harness.agent().run_handle());

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // Turn output plus the drain continuation's follow-up and reply.
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::User { content, .. }
                    if matches!(&content[0], ContentBlock::Text { text, .. } if text == "while compacting")
            )),
            "the queued follow-up was delivered: {messages:?}"
        );
        assert!(!harness.agent().has_queued_messages());
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A follow-up queued by an `agent_end` listener is delivered by one
    /// continuation even when nothing needs compaction — the queued check is
    /// the loop's own exit condition, not a side effect of the threshold
    /// branch.
    #[tokio::test]
    async fn test_agent_end_listener_follow_up_is_delivered_without_compaction() {
        let handle = std::sync::Arc::new(std::sync::Mutex::new(None::<crate::agent::RunHandle>));
        let handle_in_listener = std::sync::Arc::clone(&handle);
        let (stream_fn, _summaries) =
            ScriptedStreamFn::new(vec![ScriptedTurn::Answer(2048), ScriptedTurn::Answer(8)]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        let agent_handle = harness.agent().run_handle();
        *handle.lock().unwrap() = Some(agent_handle.clone());
        // Queue exactly once: the listener fires on every run's AgentEnd,
        // including the drain continuation's, and an unconditional queue
        // would keep the settle loop alive forever.
        let queued = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued_in_listener = std::sync::Arc::clone(&queued);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = std::sync::Arc::clone(&handle_in_listener);
            let queued = std::sync::Arc::clone(&queued_in_listener);
            Box::pin(async move {
                if matches!(event, AgentEvent::AgentEnd { .. })
                    && !queued.swap(true, std::sync::atomic::Ordering::SeqCst)
                    && let Some(handle) = handle.lock().unwrap().as_ref()
                {
                    handle.follow_up(AgentMessage::user("from agent_end"));
                }
            })
        }));

        let messages = harness.prompt("first").await.unwrap();
        // Turn output plus the drain continuation's follow-up and reply.
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::User { content, .. }
                    if matches!(&content[0], ContentBlock::Text { text, .. } if text == "from agent_end")
            )),
            "the agent_end follow-up was delivered: {messages:?}"
        );
        assert!(!harness.agent().has_queued_messages());
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A follow-up queued while the overflow compact-no-retry path compacts
    /// is delivered by one continuation after — the same delivery the
    /// threshold branch gets, reached through the overflow path this time.
    #[tokio::test]
    async fn test_overflow_compact_no_retry_delivers_queued_follow_up() {
        let handle_slot: Arc<std::sync::Mutex<Option<crate::agent::RunHandle>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_in_summary = Arc::clone(&handle_slot);
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::SilentOverflow,
            ScriptedTurn::Answer(8),
        ]);
        let stream_fn = stream_fn.on_summary(move || {
            if let Some(handle) = slot_in_summary.lock().unwrap().as_ref() {
                handle.follow_up(AgentMessage::user("while compacting"));
            }
        });
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        *handle_slot.lock().unwrap() = Some(harness.agent().run_handle());

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::User { content, .. }
                    if matches!(&content[0], ContentBlock::Text { text, .. } if text == "while compacting")
            )),
            "the queued follow-up was delivered: {messages:?}"
        );
        assert!(!harness.agent().has_queued_messages());
    }

    /// A transient provider failure is retried after the backoff and the
    /// recovered reply is returned with the failed turn — the error message
    /// leaves the transcript for the retry while the session keeps it.
    #[tokio::test]
    async fn test_retryable_error_is_retried_after_backoff() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 2,
            ..Default::default()
        });

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(matches!(
            &messages[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                error_message: Some(err),
                ..
            } if err.contains("overloaded")
        ));
        assert!(matches!(
            &messages[2],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            }
        ));
        // The transcript ends on the recovered reply with the failed turn
        // removed (the session keeps it).
        let transcript = &harness.agent().state().messages;
        assert_eq!(transcript.len(), 4, "{transcript:?}");
        assert!(
            !transcript.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            )),
            "the failed turn left the transcript: {transcript:?}"
        );
        assert!(matches!(
            transcript.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            })
        ));
    }

    /// The retry budget is per error episode: a failure that stays retryable
    /// exhausts `maxRetries` attempts, and the terminal error then settles
    /// without another retry.
    #[tokio::test]
    async fn test_retry_budget_exhaustion_keeps_terminal_error() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::RetryableError,
            ScriptedTurn::RetryableError,
            ScriptedTurn::RetryableError,
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 1,
            max_retries: 3,
            ..Default::default()
        });

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        // The original failure plus three retried failures; the fourth
        // retryable failure is terminal.
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert!(messages[1..].iter().all(|m| matches!(
            m,
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                ..
            }
        )));
        assert_eq!(harness.retry_attempt(), 0);
    }

    /// The retry lifecycle emits `auto_retry_start` then `auto_retry_end` on
    /// success, mirroring the TS session events.
    #[tokio::test]
    async fn test_auto_retry_emits_lifecycle_events() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 1,
            ..Default::default()
        });
        let events: Arc<std::sync::Mutex<Vec<RetryEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&events);
        harness.on_auto_retry(move |event| capture.lock().unwrap().push(event));

        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(
            &events[0],
            RetryEvent::Start {
                operation: RetryOperation::Turn,
                attempt: 1,
                max_attempts: 3,
                error_message,
                ..
            } if error_message.contains("overloaded")
        ));
        // The backoff elapsed, so the attempt announced itself before running.
        assert!(matches!(
            &events[1],
            RetryEvent::AttemptStart {
                operation: RetryOperation::Turn,
                attempt: 1,
            }
        ));
        assert!(matches!(
            &events[2],
            RetryEvent::End {
                operation: RetryOperation::Turn,
                success: true,
                attempt: 1,
                final_error: None,
            }
        ));
    }

    /// `HarnessHandle::abort` cancels an in-flight retry backoff and
    /// `HarnessHandle::wait_for_idle` stays pending across the whole
    /// operation (run + settle), not just the agent turn — the shared control
    /// surface a caller needs while `prompt` owns the harness exclusively.
    #[tokio::test]
    async fn test_handle_abort_cancels_backoff_and_wait_covers_settle() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 60_000,
            ..Default::default()
        });
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let start_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(start_tx)));
        let signal = std::sync::Arc::clone(&start_tx);
        harness.on_auto_retry(move |event| {
            if matches!(event, RetryEvent::Start { .. })
                && let Some(tx) = signal.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
        });
        let handle = harness.handle();
        let abort_handle = handle.clone();

        let abort_task = tokio::spawn(async move {
            start_rx.await.unwrap();
            // The harness is now in the 60s backoff; the shared handle
            // reaches it even though `prompt` owns the `&mut` borrow.
            abort_handle.abort();
        });

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        abort_task.await.unwrap();

        // The cancelled backoff never ran the retry: the turn's messages are
        // the user prompt plus the terminal error, and the lifecycle closed
        // with the cancellation reason.
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(matches!(
            &messages[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                error_message: Some(err),
                ..
            } if err.contains("overloaded")
        ));
        assert_eq!(harness.retry_attempt(), 0);

        // Fully settled: the harness-level wait resolves after the abort
        // wind-down completes.
        handle.wait_for_idle().await;
    }

    /// `HarnessHandle::wait_for_idle` stays pending while the settle loop
    /// runs after the agent turn — it must not resolve just because the
    /// agent's own run finished.
    #[tokio::test]
    async fn test_handle_wait_for_idle_stays_pending_during_backoff() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 60_000,
            ..Default::default()
        });
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let start_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(start_tx)));
        let signal = std::sync::Arc::clone(&start_tx);
        harness.on_auto_retry(move |event| {
            if matches!(event, RetryEvent::Start { .. })
                && let Some(tx) = signal.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
        });
        let handle = harness.handle();
        let wait_handle = handle.clone();
        let abort_handle = handle.clone();

        let abort_task = tokio::spawn(async move {
            start_rx.await.unwrap();
            // While the backoff is in flight, the harness is not idle.
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    wait_handle.wait_for_idle(),
                )
                .await
                .is_err(),
                "wait_for_idle must stay pending during the retry backoff"
            );
            abort_handle.abort();
        });

        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();
        abort_task.await.unwrap();
        handle.wait_for_idle().await;
    }

    /// An abort issued from the `auto_retry_start` listener must cancel the
    /// backoff it just announced — the token is armed before the event goes
    /// out. Multi-threaded runtime so the listener could observe the old
    /// token if the ordering were wrong.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_abort_from_retry_start_listener_cancels_the_new_backoff() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 60_000,
            ..Default::default()
        });
        let abort_handle = harness.handle();
        harness.on_auto_retry(move |event| {
            if matches!(event, RetryEvent::Start { .. }) {
                abort_handle.abort();
            }
        });

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        // The listener's abort landed on the armed token: the 60s backoff
        // never ran and no retry turn followed.
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(matches!(
            &messages[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                ..
            }
        ));
        assert_eq!(harness.retry_attempt(), 0);
    }

    /// A model queued mid-run reaches the next turn's provider request (the
    /// loop's prepare-next-turn seam) and is persisted once the run settles.
    #[tokio::test]
    async fn test_handle_set_model_applies_next_turn_and_persists() {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_in_stream = Arc::clone(&seen);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: Some(seen_in_stream),
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(resolved_model());
                }
            })
        }));

        let _ = harness.prompt("use the tool").await.unwrap();

        // Turn 1 streamed under the construction model; turn 2 saw the
        // queued one.
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["test".to_string(), "claude-opus".to_string()],
            "prepare_next_turn must refresh the context before the next request"
        );
        // The mutation persisted and the harness state caught up.
        assert_eq!(harness.model().id, "claude-opus");
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::ModelChange { model_id, .. } if model_id == "claude-opus"
            )),
            "the queued model change was persisted: {entries:?}"
        );
    }

    /// Per-request stream options flow from the harness turn snapshot into
    /// every provider request: idle setters apply immediately, mid-run handle
    /// setters from the next turn boundary.
    #[tokio::test]
    async fn test_stream_options_flow_from_turn_snapshot_to_requests() {
        struct OptionsStream(Arc<std::sync::Mutex<Vec<crate::types::StreamOptions>>>);
        #[async_trait::async_trait]
        impl StreamFn for OptionsStream {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                self.0.lock().unwrap().push(context.stream_options.clone());
                Ok(scripted_assistant("answer".into(), "test", "test"))
            }
        }

        let seen: Arc<std::sync::Mutex<Vec<crate::types::StreamOptions>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_in_stream = Arc::clone(&seen);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(OptionsStream(seen_in_stream)),
        );

        harness.set_stream_options(crate::types::StreamOptions {
            headers: vec![("x-gateway".into(), "a".into())],
            ..Default::default()
        });

        // Mid-run: a TurnEnd listener queues new options; the next turn's
        // requests carry them.
        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_stream_options(crate::types::StreamOptions {
                        timeout: Some(std::time::Duration::from_secs(9)),
                        ..Default::default()
                    });
                }
            })
        }));

        let _ = harness.prompt("first").await.unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].headers,
            vec![("x-gateway".to_string(), "a".to_string())]
        );
        // The first turn's TurnEnd queued the new options on the snapshot.
        assert_eq!(
            harness.stream_options().timeout,
            Some(std::time::Duration::from_secs(9)),
            "mid-run setter updated the shared snapshot"
        );

        let _ = harness.prompt("second").await.unwrap();
        let all = seen.lock().unwrap().clone();
        assert_eq!(
            all.last().unwrap().timeout,
            Some(std::time::Duration::from_secs(9)),
            "mid-run options apply to the next turn's requests"
        );
        assert_eq!(all.last().unwrap().headers, Vec::<(String, String)>::new());
    }

    /// The provider request observer maps per-attempt payloads and statuses
    /// onto the before-payload / after-response hook points.
    #[tokio::test]
    async fn test_request_observer_fires_payload_and_response_hooks() {
        let seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_payload = Arc::clone(&seen);
        let seen_status: Arc<std::sync::Mutex<Vec<u16>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_status_slot = Arc::clone(&seen_status);

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.on(
            HookPoint::BeforeProviderPayload,
            Arc::new(move |ctx: HookContext| {
                if let Some(attempt) = ctx.data.get("attempt").and_then(|v| v.as_u64()) {
                    seen_payload
                        .lock()
                        .unwrap()
                        .push(serde_json::json!({ "attempt": attempt, "payload": ctx.data.get("payload").cloned() }));
                }
                ctx
            }),
        );
        harness.on(
            HookPoint::AfterProviderResponse,
            Arc::new(move |ctx: HookContext| {
                if let Some(status) = ctx.data.get("status").and_then(|v| v.as_u64()) {
                    seen_status_slot.lock().unwrap().push(status as u16);
                }
                ctx
            }),
        );

        let observer = harness.request_observer();
        let replaced =
            observer.before_payload(2, &test_model(), &serde_json::json!({ "model": "m" }));
        // The registered handler echoed the payload unchanged; the provider
        // sends it verbatim.
        assert_eq!(replaced, Some(serde_json::json!({ "model": "m" })));
        let headers = reqwest::header::HeaderMap::new();
        observer.after_response(1, 429, &headers);
        observer.after_response(2, 200, &headers);

        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![serde_json::json!({
                "attempt": 2,
                "payload": serde_json::json!({ "model": "m" }),
            })]
        );
        assert_eq!(seen_status.lock().unwrap().clone(), vec![429, 200]);
    }

    /// before-payload handlers chain: each sees the previous handler's
    /// payload and its replacement becomes the next handler's input (TS
    /// composition); after-response carries the response headers.
    #[tokio::test]
    async fn test_payload_handlers_chain_and_headers_flow() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.on(
            HookPoint::BeforeProviderPayload,
            Arc::new(|ctx: HookContext| {
                let mut payload = ctx
                    .data
                    .get("payload")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                payload["first"] = serde_json::json!(true);
                let attempt = ctx.data.get("attempt").cloned().unwrap_or_default();
                ctx.with_data(serde_json::json!({ "attempt": attempt, "payload": payload }))
            }),
        );
        harness.on(
            HookPoint::BeforeProviderPayload,
            Arc::new(|ctx: HookContext| {
                let payload = ctx.data.get("payload").cloned().unwrap_or_default();
                // The second handler sees the first handler's mutation.
                assert_eq!(payload.get("first"), Some(&serde_json::json!(true)));
                let mut payload = payload;
                payload["second"] = serde_json::json!(true);
                let attempt = ctx.data.get("attempt").cloned().unwrap_or_default();
                ctx.with_data(serde_json::json!({ "attempt": attempt, "payload": payload }))
            }),
        );

        let observer = harness.request_observer();
        let replaced = observer
            .before_payload(1, &test_model(), &serde_json::json!({ "model": "m" }))
            .unwrap();
        assert_eq!(
            replaced,
            serde_json::json!({ "model": "m", "first": true, "second": true }),
            "mutations chain through the handlers"
        );

        // Headers flow into the after-response hook data.
        let seen_headers: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_slot = Arc::clone(&seen_headers);
        harness.on(
            HookPoint::AfterProviderResponse,
            Arc::new(move |ctx: HookContext| {
                if let Some(headers) = ctx.data.get("headers") {
                    seen_slot.lock().unwrap().push(headers.clone());
                }
                ctx
            }),
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "42".parse().unwrap());
        observer.after_response(1, 429, &headers);
        assert!(
            seen_headers
                .lock()
                .unwrap()
                .iter()
                .any(|h| h.to_string().contains("x-ratelimit-remaining")),
            "headers reach the hook data: {:?}",
            seen_headers.lock().unwrap()
        );
    }

    /// Branch-summarization retry only retries transient provider errors —
    /// auth and invalid-request failures surface immediately.
    #[test]
    fn summarization_retry_classifies_errors() {
        let transient = anyhow::anyhow!(crate::provider::ProviderError::Http {
            status: 503,
            body: "unavailable".into(),
        });
        assert!(is_transient_error(&transient));
        let transport = anyhow::anyhow!(crate::provider::ProviderError::Transport(
            "connection reset".into()
        ));
        assert!(is_transient_error(&transport));
        let auth = anyhow::anyhow!(crate::provider::ProviderError::Http {
            status: 401,
            body: "unauthorized".into(),
        });
        assert!(!is_transient_error(&auth));
        let quota = anyhow::anyhow!(crate::provider::ProviderError::Http {
            status: 429,
            body: "billing limit exceeded".into(),
        });
        // A retryable status whose body names billing is terminal.
        assert!(!is_transient_error(&quota), "quota/billing is not retried");
        let rate_limit = anyhow::anyhow!(crate::provider::ProviderError::Http {
            status: 429,
            body: "rate limit reached".into(),
        });
        assert!(is_transient_error(&rate_limit), "plain rate limit retries");
    }

    /// A `session_before_tree` hook can supply the branch summary directly,
    /// skipping the model call, and override replaceInstructions.
    #[tokio::test]
    async fn test_tree_hook_supplies_summary_and_replace_override() {
        struct CountingStream(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl StreamFn for CountingStream {
            async fn stream(
                &self,
                _context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(scripted_assistant("answer".into(), "test", "test"))
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CountingStream(Arc::clone(&calls))),
        );
        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let first_reply = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();

        harness.on(
            HookPoint::SessionBeforeTree,
            Arc::new(|ctx: HookContext| {
                ctx.with_tree_summary(BranchSummaryHookOverride {
                    summary: "hook-provided summary".into(),
                    read_files: vec!["a.rs".into()],
                    modified_files: Vec::new(),
                    usage: None,
                })
                .with_tree_replace_instructions(true)
            }),
        );
        let result = harness
            .navigate_tree_with_options(
                &first_reply,
                NavigateTreeOptions {
                    summarize: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(result.summary_entry_id.is_some());
        // The hook summary replaced the model call: only the two prompts'
        // turns streamed, no summarization request.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let summary = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::BranchSummary { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("hook summary persisted");
        assert!(summary.contains("hook-provided summary"), "{summary}");
    }

    /// A per-model stream resolver switches the provider runtime at the next
    /// turn boundary: the first provider call hits the construction-time
    /// stream, the second hits the stream for the queued model's api.
    #[tokio::test]
    async fn test_stream_resolver_switches_provider_mid_run() {
        let served_first = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let served_second = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let first_calls = std::sync::Arc::clone(&served_first);
        let second_calls = std::sync::Arc::clone(&served_second);

        // Provider A (api "test"): a tool-use turn then a plain answer.
        let provider_a = Arc::new(ToolUseStreamFn {
            call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            seen: Some(first_calls),
        }) as Arc<dyn crate::agent_loop::StreamFn>;
        let harness_stream = Arc::clone(&provider_a);
        // Provider B (api "openai_responses"): a plain answer, recording the
        // model it served.
        let provider_b: Arc<dyn crate::agent_loop::StreamFn> = Arc::new(TaggedAnswerStreamFn {
            served: second_calls,
        });
        let resolver: crate::agent_loop::StreamResolver = Arc::new(move |model: &Model| {
            if model.api == "openai_responses" {
                Ok(Arc::clone(&provider_b))
            } else {
                Ok(Arc::clone(&provider_a))
            }
        });

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            harness_stream,
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]))
        .with_stream_resolver(resolver);

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(Model {
                        provider: "openai".into(),
                        api: "openai_responses".into(),
                        id: "gpt-responses".into(),
                        context_window: 200_000,
                        max_tokens: 16_384,
                        thinking: ThinkingKind::None,
                        metadata: Default::default(),
                    });
                }
            })
        }));

        let _ = harness.prompt("use the tool").await.unwrap();

        // Turn 1 went to provider A under the construction model; turn 2 went
        // to provider B under the queued model — a real runtime switch, not
        // just a context field change.
        assert_eq!(
            *served_first.lock().unwrap(),
            vec!["test".to_string()],
            "turn 1 must hit the construction-time provider"
        );
        assert_eq!(
            *served_second.lock().unwrap(),
            vec!["gpt-responses".to_string()],
            "turn 2 must hit the queued model's provider"
        );
        assert_eq!(harness.model().id, "gpt-responses");
    }

    /// A failed durable write keeps the mutation in the queue (and the tail
    /// after it) for the next flush — nothing is dropped, and a later flush
    /// retries the suffix until it lands. The recovered run must also be
    /// served by the queued model: the failed write must not revert the next
    /// provider request to the persisted model.
    #[tokio::test]
    async fn test_flush_keeps_unpersisted_mutation_on_failure() {
        let storage = MemStorage::new();
        *storage.fail_model_id.lock().unwrap() = Some("model-b".into());
        let session = Session::new(storage);
        let served: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let served_in_stream = Arc::clone(&served);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: Some(served_in_stream),
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(Model {
                        provider: "test".into(),
                        api: "test".into(),
                        id: "model-a".into(),
                        context_window: 100_000,
                        max_tokens: 8_192,
                        thinking: ThinkingKind::None,
                        metadata: Default::default(),
                    });
                    handle.set_model(Model {
                        provider: "test".into(),
                        api: "test".into(),
                        id: "model-b".into(),
                        context_window: 100_000,
                        max_tokens: 8_192,
                        thinking: ThinkingKind::None,
                        metadata: Default::default(),
                    });
                }
            })
        }));

        async fn model_changes(harness: &AgentHarness<MemStorage>) -> Vec<String> {
            let entries = harness
                .session()
                .storage()
                .get_entries(Default::default())
                .await
                .unwrap();
            entries
                .iter()
                .filter_map(|e| match e {
                    SessionTreeEntry::ModelChange { model_id, .. } => Some(model_id.clone()),
                    _ => None,
                })
                .collect()
        }

        // model_a persists (flushed at the first turn boundary); model_b's
        // append fails at the next turn boundary and surfaces the error.
        assert!(harness.prompt("use the tool").await.is_err());
        assert_eq!(model_changes(&harness).await, vec!["model-a".to_string()]);

        // The failed write stayed queued: the next run's flush retries it and
        // both changes land — and the run itself is served by the queued
        // model, not the persisted one.
        *harness.session().storage().fail_model_id.lock().unwrap() = None;
        let _ = harness.prompt("again").await.unwrap();
        assert_eq!(
            model_changes(&harness).await,
            vec!["model-a".to_string(), "model-b".to_string()]
        );
        assert_eq!(
            *served.lock().unwrap(),
            vec!["test".to_string(), "model-b".to_string()],
            "the recovered run must be served by the queued model, not the \
             persisted one"
        );
    }

    /// A model switch keyed on api with the same model id still swaps the
    /// provider runtime — the resolver discriminates on api, not id.
    #[tokio::test]
    async fn test_same_id_different_api_switches_stream() {
        let served_first = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let served_second = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let first_calls = std::sync::Arc::clone(&served_first);
        let second_calls = std::sync::Arc::clone(&served_second);
        let provider_a = Arc::new(ToolUseStreamFn {
            call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            seen: Some(first_calls),
        }) as Arc<dyn crate::agent_loop::StreamFn>;
        let provider_b: Arc<dyn crate::agent_loop::StreamFn> = Arc::new(TaggedAnswerStreamFn {
            served: second_calls,
        });
        let harness_stream = Arc::clone(&provider_a);
        let resolver: crate::agent_loop::StreamResolver = Arc::new(move |model: &Model| {
            if model.api == "openai_responses" {
                Ok(Arc::clone(&provider_b))
            } else {
                Ok(Arc::clone(&provider_a))
            }
        });
        let same_id = |api: &str| Model {
            provider: "openai".into(),
            api: api.into(),
            id: "same-id".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        };

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            same_id("openai_completions"),
            harness_stream,
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]))
        .with_stream_resolver(resolver);

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(same_id("openai_responses"));
                }
            })
        }));

        let _ = harness.prompt("use the tool").await.unwrap();
        // The id stayed "same-id" across both calls; the stream changed with
        // the api.
        assert_eq!(*served_first.lock().unwrap(), vec!["same-id".to_string()]);
        assert_eq!(*served_second.lock().unwrap(), vec!["same-id".to_string()]);
    }

    /// A model switch with the same id but a different provider updates the
    /// loop context — the resolver keyed on provider routes the next turn
    /// differently.
    #[tokio::test]
    async fn test_same_id_different_provider_updates_context() {
        let served_first = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let served_second = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let first_calls = std::sync::Arc::clone(&served_first);
        let second_calls = std::sync::Arc::clone(&served_second);
        let provider_a = Arc::new(ToolUseStreamFn {
            call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            seen: Some(first_calls),
        }) as Arc<dyn crate::agent_loop::StreamFn>;
        let provider_b: Arc<dyn crate::agent_loop::StreamFn> = Arc::new(TaggedAnswerStreamFn {
            served: second_calls,
        });
        let harness_stream = Arc::clone(&provider_a);
        let resolver: crate::agent_loop::StreamResolver = Arc::new(move |model: &Model| {
            if model.provider == "other" {
                Ok(Arc::clone(&provider_b))
            } else {
                Ok(Arc::clone(&provider_a))
            }
        });
        let same_id = |provider: &str| Model {
            provider: provider.into(),
            api: "test".into(),
            id: "same-id".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        };

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            same_id("test"),
            harness_stream,
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]))
        .with_stream_resolver(resolver);

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(same_id("other"));
                }
            })
        }));

        let _ = harness.prompt("use the tool").await.unwrap();
        // The provider change reached the loop context: the second turn was
        // routed by provider.
        assert_eq!(*served_first.lock().unwrap(), vec!["same-id".to_string()]);
        assert_eq!(*served_second.lock().unwrap(), vec!["same-id".to_string()]);
    }

    /// A resolver failure on the first turn is a terminal error message with
    /// the normal lifecycle — not a run-level panic.
    #[tokio::test]
    async fn test_resolver_failure_on_first_turn_is_terminal() {
        let resolver: crate::agent_loop::StreamResolver =
            Arc::new(|_: &Model| Err(anyhow::anyhow!("no provider runtime for this model")));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_stream_resolver(resolver);

        let messages = harness.prompt("hi").await.unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(matches!(
            &messages[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                error_message: Some(e),
                ..
            } if e.contains("failed to resolve provider runtime")
        ));
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A resolver failure after a mid-run model switch terminates the next
    /// turn cleanly.
    #[tokio::test]
    async fn test_resolver_failure_after_model_switch_is_terminal() {
        let provider_a = Arc::new(ToolUseStreamFn {
            call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            seen: None,
        }) as Arc<dyn crate::agent_loop::StreamFn>;
        let harness_stream = Arc::clone(&provider_a);
        let resolver: crate::agent_loop::StreamResolver = Arc::new(move |model: &Model| {
            if model.id == "broken" {
                Err(anyhow::anyhow!("unsupported model"))
            } else {
                Ok(Arc::clone(&provider_a))
            }
        });
        let broken = Model {
            provider: "test".into(),
            api: "test".into(),
            id: "broken".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        };

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            harness_stream,
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]))
        .with_stream_resolver(resolver);

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            let broken = broken.clone();
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(broken);
                }
            })
        }));

        let messages = harness.prompt("use the tool").await.unwrap();
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    error_message: Some(e),
                    ..
                } if e.contains("failed to resolve provider runtime")
            )),
            "{messages:?}"
        );
        assert!(matches!(
            messages.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                ..
            })
        ));
    }

    /// Restore pulls the model from the session into the shared runtime, so a
    /// prompt after restore uses the restored provider — and a resolver
    /// failure for it stays terminal across the restore.
    #[tokio::test]
    async fn test_restore_then_prompt_uses_restored_runtime() {
        let served: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let served_in_stream = Arc::clone(&served);
        let responses = Model {
            provider: "openai".into(),
            api: "openai_responses".into(),
            id: "gpt-responses".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        };
        let provider_a = Arc::new(ToolUseStreamFn {
            call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            seen: None,
        }) as Arc<dyn crate::agent_loop::StreamFn>;
        let harness_stream = Arc::clone(&provider_a);
        let provider_b: Arc<dyn crate::agent_loop::StreamFn> = Arc::new(TaggedAnswerStreamFn {
            served: served_in_stream,
        });
        let resolver: crate::agent_loop::StreamResolver = Arc::new(move |model: &Model| {
            if model.api == "openai_responses" {
                Ok(Arc::clone(&provider_b))
            } else {
                Ok(Arc::clone(&provider_a))
            }
        });

        // Harness 1 persists the model change.
        let mut h1 = AgentHarness::new(
            Session::new(MemStorage::new()),
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        h1.set_model(responses.clone()).await.unwrap();
        let entries = h1
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();

        // Harness 2 restores the session and prompts under the restored model.
        let mut h2 = AgentHarness::new(
            Session::new(MemStorage::from_entries(entries)),
            "You are a test assistant.",
            test_model(),
            harness_stream,
        )
        .with_stream_resolver(resolver)
        .with_model_resolver({
            let responses = responses.clone();
            move |mref: &crate::session::SessionModelRef| {
                (mref.provider == "openai" && mref.model_id == "gpt-responses")
                    .then(|| responses.clone())
            }
        });
        h2.restore().await.unwrap();
        assert_eq!(h2.model().id, "gpt-responses");

        let _ = h2.prompt("hi").await.unwrap();
        assert_eq!(
            *served.lock().unwrap(),
            vec!["gpt-responses".to_string()],
            "the restored model's provider served the prompt"
        );
    }

    /// A resolver failure survives restore: the restored model still fails
    /// to resolve on the next prompt, terminal rather than silent.
    #[tokio::test]
    async fn test_resolver_failure_survives_restore() {
        let broken = Model {
            provider: "test".into(),
            api: "test".into(),
            id: "broken".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        };
        let failing =
            |model: &Model| -> Result<Arc<dyn crate::agent_loop::StreamFn>, anyhow::Error> {
                if model.id == "broken" {
                    Err(anyhow::anyhow!("unsupported model"))
                } else {
                    Ok(Arc::new(TestStreamFn) as Arc<dyn crate::agent_loop::StreamFn>)
                }
            };
        let resolver: crate::agent_loop::StreamResolver = Arc::new(failing);

        let mut h1 = AgentHarness::new(
            Session::new(MemStorage::new()),
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_stream_resolver(resolver);
        h1.set_model(broken.clone()).await.unwrap();
        let entries = h1
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();

        let mut h2 = AgentHarness::new(
            Session::new(MemStorage::from_entries(entries)),
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_stream_resolver(Arc::new(failing))
        .with_model_resolver({
            let broken = broken.clone();
            move |mref: &crate::session::SessionModelRef| {
                (mref.provider == "test" && mref.model_id == "broken").then(|| broken.clone())
            }
        });
        h2.restore().await.unwrap();

        let messages = h2.prompt("hi").await.unwrap();
        let terminal = match messages.last() {
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                error_message: Some(e),
                ..
            }) => e.contains("failed to resolve provider runtime"),
            _ => false,
        };
        assert!(terminal, "{messages:?}");
        assert_eq!(h2.phase(), AgentHarnessPhase::Idle);
    }

    /// A follow-up queued by an `agent_end` listener together with a model
    /// change is delivered on the first continuation turn under the new model
    /// — the runtime snapshot applies before any new run starts.
    #[tokio::test]
    async fn test_continuation_first_turn_uses_queued_model() {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_in_stream = Arc::clone(&seen);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: Some(seen_in_stream),
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        let queued = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued_in_listener = std::sync::Arc::clone(&queued);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let queued = std::sync::Arc::clone(&queued_in_listener);
            Box::pin(async move {
                if matches!(event, AgentEvent::AgentEnd { .. })
                    && !queued.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    handle.set_model(resolved_model());
                    handle.follow_up(AgentMessage::user("follow up"));
                }
            })
        }));

        let messages = harness.prompt("use the tool").await.unwrap();
        // Turns 1-2 (tool use + reply) under the construction model; the
        // follow-up continuation's first turn under the queued model.
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "test".to_string(),
                "test".to_string(),
                "claude-opus".to_string()
            ],
            "the follow-up's first turn must already use the queued model"
        );
        assert!(messages.len() >= 4, "{messages:?}");
        assert!(messages.iter().any(|m| matches!(
            m,
            AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "follow up")
        )));
    }

    /// The thinking-level setter persists a `thinking_level_change` entry
    /// that a later restore projects back onto the agent.
    #[tokio::test]
    async fn test_set_thinking_level_persists_and_restores() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness
            .set_thinking_level(Some("high".into()))
            .await
            .unwrap();
        assert_eq!(
            harness.agent().state().thinking_level.as_deref(),
            Some("high")
        );

        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::ThinkingLevelChange {
                    thinking_level: l,
                    ..
                } if l == "high"
            )),
            "the thinking level change was persisted: {entries:?}"
        );

        // A fresh harness over the same session restores the tier.
        let mut restored = AgentHarness::new(
            Session::new(MemStorage::from_entries(entries)),
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        restored.restore().await.unwrap();
        assert_eq!(
            restored.agent().state().thinking_level.as_deref(),
            Some("high")
        );
    }

    /// Context overflow never enters the agent-level auto-retry — it goes to
    /// compaction, and no retry lifecycle events fire.
    #[tokio::test]
    async fn test_overflow_error_is_not_auto_retried() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 1,
            ..Default::default()
        });
        let retries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = std::sync::Arc::clone(&retries);
        harness.on_auto_retry(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });
        let messages = harness.prompt("second").await.unwrap();
        // Overflow compact-and-retry, not agent-level retry.
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(retries.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(harness.retry_attempt(), 0);
    }

    /// A failed summarization (Error/Aborted terminal, or an empty summary)
    /// must not persist a compaction entry nor rewrite the transcript — the
    /// compacted prefix would otherwise be replaced with nothing and lost.
    #[tokio::test]
    async fn test_compact_rejects_failed_summary_without_persisting() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(AbortedSummaryStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        assert!(harness.needs_compaction());

        let entries_before = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();
        let transcript_before = harness.agent_mut().state().messages.len();

        let err = harness.compact(None).await.unwrap_err();
        assert!(
            err.to_string().contains("summarization failed (aborted)"),
            "{}",
            err
        );

        // No compaction entry was persisted.
        let entries_after = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();
        assert_eq!(entries_after, entries_before);

        // The transcript is untouched: the compacted prefix survives.
        assert_eq!(
            harness.agent_mut().state().messages.len(),
            transcript_before
        );
    }

    /// A `SessionBeforeCompact` hook can cancel the run: nothing is persisted
    /// and the transcript is left intact. Mirrors the TS `cancel` branch.
    #[tokio::test]
    async fn test_before_compact_hook_can_cancel() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        let entries_before = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| ctx.with_cancel_compaction()),
        );

        let err = harness.compact(None).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");

        // No compaction entry was persisted.
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries(Default::default())
                .await
                .unwrap()
                .len(),
            entries_before
        );
        // The transcript is untouched.
        assert_eq!(harness.agent_mut().state().messages.len(), 4);
    }

    /// A `SessionBeforeCompact` hook can supply the summary directly: the
    /// summarization model is never called, and the persisted entry carries
    /// `fromHook` and the hook's `details`. Mirrors the TS `compaction` branch.
    #[tokio::test]
    async fn test_before_compact_hook_can_supply_summary() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingStreamFn {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl StreamFn for CountingStreamFn {
            async fn stream(
                &self,
                _context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "MUST NOT BE USED".into(),
                        signature: None,
                    }],
                    model: "test".into(),
                    provider: "test".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Stop),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            }
        }

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CountingStreamFn {
                calls: Arc::clone(&calls),
            }),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| {
                ctx.with_compact_override(BeforeCompactOverride {
                    summary: "hook-authored summary".into(),
                    tokens_before: 90_000,
                    first_kept_entry_id: None,
                    retained_tail: vec![],
                    details: Some(serde_json::json!({"files": ["a.rs", "b.rs"]})),
                    usage: Some(Usage {
                        total_tokens: 7,
                        ..Default::default()
                    }),
                })
            }),
        );

        let result = harness.compact(None).await.expect("hook-supplied compact");
        assert_eq!(result.summary, "hook-authored summary");
        // The summarization model was never called.
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // The persisted entry carries fromHook and the hook's details.
        let compaction = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Compaction {
                    summary,
                    from_hook,
                    details,
                    usage,
                    tokens_before,
                    ..
                } => Some((
                    summary.clone(),
                    *from_hook,
                    details.clone(),
                    usage.clone(),
                    *tokens_before,
                )),
                _ => None,
            })
            .expect("compaction persisted");
        assert_eq!(compaction.0, "hook-authored summary");
        assert_eq!(compaction.1, Some(true));
        assert_eq!(
            compaction.2,
            Some(serde_json::json!({"files": ["a.rs", "b.rs"]}))
        );
        assert_eq!(compaction.3.map(|u| u.total_tokens), Some(7));
        // tokens_before is persisted verbatim from the hook, not recomputed.
        assert_eq!(compaction.4, 90_000);

        // The returned result mirrors the hook's authorship, so callers need
        // not re-read storage to recover it.
        assert_eq!(result.summary, "hook-authored summary");
        assert_eq!(result.usage.map(|u| u.total_tokens), Some(7));
        assert_eq!(
            result.details,
            Some(serde_json::json!({"files": ["a.rs", "b.rs"]}))
        );
        assert!(result.retained_tail.is_empty());
    }

    /// The before-compact hook receives the TS-shaped `preparation` and the
    /// session `branchEntries`, so it can decide on the specific content being
    /// compacted rather than blind. Mirrors the TS `SessionBeforeCompactEvent`.
    #[tokio::test]
    async fn test_before_compact_hook_receives_preparation_and_branch_entries() {
        use std::sync::Mutex;

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        // Build a real session (two long prompts → two user/assistant pairs
        // persisted as Message entries) so `branchEntries` is non-empty. The
        // keep-recent budget sits between one and two turns, so the cut lands
        // mid-transcript: a non-empty prefix is summarized and a non-empty tail
        // with a real first-kept entry id is retained.
        let long = "x".repeat(2048);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        let captured = Arc::new(Mutex::new(serde_json::Value::Null));
        harness.on(HookPoint::SessionBeforeCompact, {
            let captured = Arc::clone(&captured);
            Arc::new(move |ctx: HookContext| {
                *captured.lock().unwrap() = ctx.data.clone();
                ctx
            })
        });

        harness.compact(None).await.expect("compact");

        let data = captured.lock().unwrap().clone();
        assert_eq!(
            data.get("type").and_then(|v| v.as_str()),
            Some("session_before_compact"),
            "the typed event carries the TS discriminator: {data}"
        );
        // `customInstructions` is omitted (None) when compact() takes none —
        // the field is part of the contract but absent on the wire, matching
        // TS optionality.
        assert!(data.get("customInstructions").is_none());

        let preparation = data
            .get("preparation")
            .expect("hook data carries preparation");
        // TS `CompactionPreparation` field names and shapes.
        assert!(
            preparation
                .get("firstKeptEntryId")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "firstKeptEntryId is the real id of the first retained entry: {preparation}"
        );
        assert!(
            preparation
                .get("tokensBefore")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0,
            "preparation reports the pre-compaction token count: {preparation}"
        );
        assert!(
            !preparation
                .get("isSplitTurn")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            "this transcript's cut lands on a whole-turn boundary"
        );
        let messages_to_summarize = preparation
            .get("messagesToSummarize")
            .and_then(|v| v.as_array())
            .expect("preparation carries messagesToSummarize");
        assert!(
            !messages_to_summarize.is_empty(),
            "messagesToSummarize holds the compacted prefix"
        );
        let retained_tail = preparation
            .get("retainedTail")
            .and_then(|v| v.as_array())
            .expect("preparation carries retainedTail");
        assert!(
            !retained_tail.is_empty(),
            "retainedTail holds the kept suffix"
        );
        assert!(
            preparation
                .get("turnPrefixMessages")
                .and_then(|v| v.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(false),
            "a whole-turn cut contributes no turn prefix"
        );
        // No prior compaction on this branch.
        assert!(
            preparation.get("previousSummary").is_none(),
            "previousSummary is absent when the branch never compacted"
        );
        assert!(
            preparation.get("fileOps").is_some(),
            "fileOps is always present"
        );
        assert!(
            preparation.get("settings").is_some(),
            "settings is always present"
        );

        let branch_entries = data
            .get("branchEntries")
            .and_then(|v| v.as_array())
            .expect("hook data carries branchEntries");
        assert!(
            !branch_entries.is_empty(),
            "branchEntries holds the session entries of the branch"
        );
    }

    /// `compact(Some(_))` folds custom instructions into the summarization
    /// prompt on the default (no-override) path — they must not be silently
    /// dropped merely because no hook supplied a summary.
    #[tokio::test]
    async fn test_compact_custom_instructions_reach_summarization_prompt() {
        use std::sync::Mutex;

        struct CaptureStreamFn {
            seen: Arc<Mutex<String>>,
        }

        #[async_trait::async_trait]
        impl StreamFn for CaptureStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if let Some(AgentMessage::User { content, .. }) = context.messages.first() {
                    for b in content {
                        if let ContentBlock::Text { text, .. } = b {
                            *self.seen.lock().unwrap() = text.clone();
                        }
                    }
                }
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "summary".into(),
                        signature: None,
                    }],
                    model: "test".into(),
                    provider: "test".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Stop),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            }
        }

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let seen = Arc::new(Mutex::new(String::new()));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CaptureStreamFn {
                seen: Arc::clone(&seen),
            }),
        );
        // A transcript large enough to compact a non-empty prefix past the
        // keep-recent budget, so the summarization model is actually invoked.
        let long = "x".repeat(2048);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        harness
            .compact(Some("emphasize the auth module"))
            .await
            .expect("compact");

        let prompt = seen.lock().unwrap().clone();
        assert!(
            prompt.contains("Additional focus: emphasize the auth module"),
            "custom instructions reach the summarization prompt, not dropped: {prompt}"
        );
    }

    /// The default (no-override) summarization path consumes the
    /// [`CompactionPreparation`]: file operations extracted from the compacted
    /// prefix are appended to the summary text and returned as `details`
    /// (`readFiles`/`modifiedFiles`), mirroring TS `compact`. A read call on a
    /// file that was also edited/written is classified as modified, not read.
    #[tokio::test]
    async fn test_compact_model_path_folds_file_operations() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let tool_use = |id: &str, name: &str, path: &str| ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({"path": path}),
            thought_signature: None,
        };
        // user1 alone exceeds the keep-recent budget, so the cut lands after
        // the assistant tool-use turn — putting the tool calls in the prefix
        // that gets summarized (and thus in `fileOps`).
        let long = "x".repeat(4096);
        let assistant = AgentMessage::Assistant {
            content: vec![
                tool_use("1", "read", "a.rs"),
                tool_use("2", "read", "c.rs"),
                tool_use("3", "edit", "a.rs"),
                tool_use("4", "write", "b.rs"),
            ],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness.agent_mut().replace_transcript(vec![
            AgentMessage::user(&long),
            assistant,
            AgentMessage::user(&long),
            AgentMessage::user("recent tail"),
        ]);
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        let result = harness.compact(None).await.expect("compact");
        // `a.rs` is read and edited → modified wins; `c.rs` is read-only.
        assert!(
            result.summary.contains("<read-files>\nc.rs\n</read-files>"),
            "read-only files are appended to the summary: {summary}",
            summary = result.summary
        );
        assert!(
            result
                .summary
                .contains("<modified-files>\na.rs\nb.rs\n</modified-files>"),
            "edited ∪ written files are appended to the summary: {summary}",
            summary = result.summary
        );
        assert_eq!(
            result.details,
            Some(serde_json::json!({
                "readFiles": ["c.rs"],
                "modifiedFiles": ["a.rs", "b.rs"],
            })),
            "details carries the computed file lists"
        );
    }

    /// A repeated compaction folds the prior summary in once (as
    /// `previousSummary`) and excludes the synthetic `summary_message` from
    /// `messagesToSummarize` — mirroring TS, whose `messagesToSummarize` starts
    /// at the boundary's first kept entry, not the compaction entry. The prior
    /// summary must not appear twice in the summarization prompt.
    #[tokio::test]
    async fn test_repeated_compaction_does_not_duplicate_prior_summary() {
        use std::sync::Mutex;

        struct CaptureStreamFn {
            seen: Arc<Mutex<String>>,
        }

        #[async_trait::async_trait]
        impl StreamFn for CaptureStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if let Some(AgentMessage::User { content, .. }) = context.messages.first() {
                    for b in content {
                        if let ContentBlock::Text { text, .. } = b {
                            *self.seen.lock().unwrap() = text.clone();
                        }
                    }
                }
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "prior session covered the API".into(),
                        signature: None,
                    }],
                    model: "test".into(),
                    provider: "test".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Stop),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            }
        }

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let seen = Arc::new(Mutex::new(String::new()));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CaptureStreamFn {
                seen: Arc::clone(&seen),
            }),
        );
        let long = "x".repeat(4096);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        // First compaction establishes a prior summary on the branch.
        harness.compact(None).await.expect("first compact");
        // Grow the transcript so a second compaction has a non-empty prefix.
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.compact(None).await.expect("second compact");

        let prompt = seen.lock().unwrap().clone();
        // The prior summary is folded in once as <previous-summary> context.
        assert!(
            prompt
                .contains("<previous-summary>\nprior session covered the API\n</previous-summary>"),
            "previousSummary is folded into the prompt: {prompt}"
        );
        // The synthetic summary carrier is excluded from messagesToSummarize —
        // its wrapper must not leak into the prompt as a transcript line.
        assert!(
            !prompt.contains(
                "The conversation history before this point was compacted into the following summary:"
            ),
            "the prior summary message is not double-counted: {prompt}"
        );
    }

    /// A hook override with an empty summary must not persist a compaction:
    /// the compacted prefix would be replaced with nothing and lost. Same
    /// invariant the model path enforces for empty model summaries.
    #[tokio::test]
    async fn test_before_compact_hook_empty_summary_rejected() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        let entries_before = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| {
                ctx.with_compact_override(BeforeCompactOverride {
                    summary: "   \n\t".into(),
                    tokens_before: 0,
                    first_kept_entry_id: None,
                    retained_tail: vec![],
                    details: None,
                    usage: None,
                })
            }),
        );

        let err = harness.compact(None).await.unwrap_err();
        assert!(err.to_string().contains("empty summary"), "{err}");

        // No compaction entry was persisted and the transcript is intact.
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries(Default::default())
                .await
                .unwrap()
                .len(),
            entries_before
        );
        assert_eq!(harness.agent_mut().state().messages.len(), 4);
    }

    /// A hook that overrides only the summary keeps the harness-computed tail
    /// by passing `preparation.retained_tail` through verbatim. The required
    /// `retained_tail` field makes erasure an explicit `vec![]`, not an
    /// accidental omission — so a summary-only override still retains context.
    #[tokio::test]
    async fn test_before_compact_hook_passes_through_retained_tail() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let long = "x".repeat(2048);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| {
                // Pass the harness-computed tail through; supply a fresh summary
                // and the preparation's token count so the override is complete.
                let preparation = ctx
                    .data
                    .get("preparation")
                    .unwrap_or(&serde_json::Value::Null);
                let retained_tail: Vec<AgentMessage> = preparation
                    .get("retainedTail")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let tokens_before = preparation
                    .get("tokensBefore")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                ctx.with_compact_override(BeforeCompactOverride {
                    summary: "hook summary that keeps the tail".into(),
                    tokens_before,
                    first_kept_entry_id: None,
                    retained_tail,
                    usage: None,
                    details: None,
                })
            }),
        );

        let result = harness.compact(None).await.expect("compact");
        // The hook's fresh summary replaces the prefix; the passed-through tail
        // survives, so the rebuilt transcript is summary + retained messages.
        assert!(
            !result.retained_tail.is_empty(),
            "tail was retained, not erased"
        );
        assert_eq!(
            harness.agent().state().messages.len(),
            1 + result.retained_tail.len(),
            "transcript = summary + retained tail"
        );
    }

    #[tokio::test]
    async fn test_compact_drains_more_events_than_channel_capacity() {
        // The summarization channel caps at 64 events; ChattyStreamFn emits
        // 200. A harness that drains only after the producer returns would
        // deadlock here (the test would hang and time out).
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ChattyStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());

        let result = harness
            .compact(None)
            .await
            .expect("compact must not deadlock");
        assert_eq!(result.summary, "summary");
    }

    #[tokio::test]
    async fn test_compaction_boundary_survives_session_restore() {
        use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let meta = || JsonlSessionMetadata {
            id: "s".into(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        };

        let stale_assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };

        // Compact in a first harness over an on-disk session.
        {
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let session = Session::new(storage);
            let mut harness = AgentHarness::new(
                session,
                "You are a test assistant.",
                test_model(),
                Arc::new(TestStreamFn),
            );
            harness
                .agent_mut()
                .replace_transcript(compactable_transcript());
            harness.set_compaction_settings(compact_test_settings());
            let result = harness.compact(None).await.unwrap();
            assert_eq!(result.tokens_before, 90_000);
        }

        // Reopen the session from disk: the compaction entry survived; the
        // kept segment behind it is reconstructed by walking the tree from
        // its first-kept entry id, not read off the boundary.
        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let entries = storage.get_entries(Default::default()).await.unwrap();
        let boundary = entries.iter().find_map(|e| match e {
            SessionTreeEntry::Compaction {
                summary,
                tokens_before,
                ..
            } => Some((summary.clone(), *tokens_before)),
            _ => None,
        });
        assert_eq!(boundary, Some(("Test response".to_string(), 90_000)));

        // A fresh harness over the restored session recovers the boundary, so
        // a transcript whose usage predates the compaction cannot anchor.
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.recover_boundary().await.unwrap();
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), stale_assistant.clone()]);
        assert!(!harness.needs_compaction());

        // Without recovery the stale usage anchors and the threshold trips.
        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), stale_assistant]);
        assert!(harness.needs_compaction());
    }

    #[tokio::test]
    async fn test_restore_rebuilds_transcript_from_session() {
        use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let meta = || JsonlSessionMetadata {
            id: "s".into(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        };

        // Run a turn, compact, run another turn — all over an on-disk
        // session. The first prompt is long and the keep-recent budget
        // narrow, so the cut retains only the first turn's assistant reply:
        // the post-compaction transcript is summary + assistant.
        let expected;
        {
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let session = Session::new(storage);
            let mut harness = AgentHarness::new(
                session,
                "You are a test assistant.",
                test_model(),
                Arc::new(TestStreamFn),
            );
            harness.set_compaction_settings(CompactionSettings {
                keep_recent_tokens: 100,
                ..Default::default()
            });
            harness.prompt(&"f".repeat(2048)).await.unwrap();
            harness.compact(None).await.unwrap();
            harness.prompt("second").await.unwrap();
            expected = serde_json::to_value(&harness.agent().state().messages).unwrap();
        }

        // A fresh harness restores the full transcript: summary, retained
        // tail, and the post-compaction messages.
        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.restore().await.unwrap();

        fn text_of(m: &AgentMessage) -> &str {
            match m {
                AgentMessage::User { content, .. } | AgentMessage::Assistant { content, .. } => {
                    match &content[0] {
                        ContentBlock::Text { text, .. } => text.as_str(),
                        _ => "",
                    }
                }
                _ => "",
            }
        }
        let messages = &harness.agent().state().messages;
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(
            text_of(&messages[0]),
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nTest response\n</summary>"
        );
        assert!(matches!(&messages[1], AgentMessage::Assistant { .. }));
        assert_eq!(text_of(&messages[2]), "second");
        assert!(matches!(&messages[3], AgentMessage::Assistant { .. }));

        // The restored transcript equals the post-compaction one exactly,
        // summary timestamp included.
        let restored = serde_json::to_value(messages).unwrap();
        assert_eq!(restored, expected);

        // The estimation boundary came along: needs_compaction works without
        // a separate recover_boundary() call.
        assert!(!harness.needs_compaction());
    }

    /// A hook-supplied retained tail persists with the boundary: the restored
    /// transcript replays it even though the hook's messages were never
    /// session entries, so a first-kept tree walk could never find them.
    #[tokio::test]
    async fn test_hook_supplied_retained_tail_survives_restore() {
        use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let meta = || JsonlSessionMetadata {
            id: "s".into(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        };

        let expected;
        {
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let session = Session::new(storage);
            let mut harness = AgentHarness::new(
                session,
                "You are a test assistant.",
                test_model(),
                Arc::new(TestStreamFn),
            );
            harness.on(
                HookPoint::SessionBeforeCompact,
                Arc::new(|ctx: HookContext| {
                    ctx.with_compact_override(BeforeCompactOverride {
                        summary: "hook summary".into(),
                        tokens_before: 90_000,
                        first_kept_entry_id: None,
                        retained_tail: vec![
                            AgentMessage::user("hook-kept question"),
                            AgentMessage::user("hook-kept answer"),
                        ],
                        details: None,
                        usage: None,
                    })
                }),
            );
            harness
                .agent_mut()
                .replace_transcript(compactable_transcript());
            harness.set_compaction_settings(compact_test_settings());
            harness.compact(None).await.unwrap();
            expected = serde_json::to_value(&harness.agent().state().messages).unwrap();
        }

        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.restore().await.unwrap();

        let messages = &harness.agent().state().messages;
        // Summary carrier + both hook-authored tail messages.
        assert_eq!(messages.len(), 3, "{messages:?}");
        // The restored transcript equals the post-compaction one exactly.
        let restored = serde_json::to_value(messages).unwrap();
        assert_eq!(restored, expected);
    }

    /// Restore projects every message-producing entry variant — messages,
    /// custom messages, branch summaries — and replays the run configuration
    /// the path carries (reasoning tier, model, active tools) without
    /// appending new entries. Display/state entries stay out of the
    /// transcript.
    #[tokio::test]
    async fn test_restore_projects_all_entry_variants_and_settings() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let storage = session.storage();

        storage
            .append_entry(&SessionTreeEntry::ThinkingLevelChange {
                id: "t1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                thinking_level: "high".into(),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: Some("t1".into()),
                timestamp: chrono::Utc::now(),
                provider: "anthropic".into(),
                model_id: "claude-opus".into(),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::Message {
                id: "m1".into(),
                parent_id: Some("mc".into()),
                timestamp: chrono::Utc::now(),
                message: AgentMessage::user("hello"),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::CustomMessage {
                id: "cm1".into(),
                parent_id: Some("m1".into()),
                timestamp: chrono::Utc::now(),
                custom_type: "notice".into(),
                content: vec![ContentBlock::Text {
                    text: "heads up".into(),
                    signature: None,
                }],
                details: None,
                display: true,
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::BranchSummary {
                id: "bs1".into(),
                parent_id: Some("cm1".into()),
                timestamp: chrono::Utc::now(),
                from_id: "m1".into(),
                summary: "explored a side branch".into(),
                details: None,
                usage: None,
                from_hook: None,
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::Message {
                id: "m2".into(),
                parent_id: Some("bs1".into()),
                timestamp: chrono::Utc::now(),
                message: AgentMessage::user("after"),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::ActiveToolsChange {
                id: "at1".into(),
                parent_id: Some("m2".into()),
                timestamp: chrono::Utc::now(),
                active_tool_names: vec!["echo".into()],
            })
            .await
            .unwrap();

        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_tools(two_tools())
        .with_model_resolver(test_model_resolver());
        harness.restore().await.unwrap();

        let messages = &harness.agent().state().messages;
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(matches!(&messages[0], AgentMessage::User { .. }));
        match &messages[1] {
            AgentMessage::Custom {
                custom_type,
                display,
                ..
            } => {
                assert_eq!(custom_type, "notice");
                assert!(display);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
        match &messages[2] {
            AgentMessage::User { content, .. } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(
                    text,
                    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\nexplored a side branch</summary>"
                ),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
        assert!(matches!(&messages[3], AgentMessage::User { .. }));

        // The reasoning tier on the path reaches the agent, the resolver
        // swaps in the session's model, and the active-tool selection narrows
        // the mounted set — restore replays the run configuration without
        // appending anything.
        assert_eq!(
            harness.agent().state().thinking_level.as_deref(),
            Some("high")
        );
        assert_eq!(harness.model().id, "claude-opus");
        assert_eq!(harness.agent().state().model.id, "claude-opus");
        assert_eq!(mounted_names(&harness), ["echo"]);
        assert_eq!(harness.active_tool_names(), Some(&["echo".to_string()][..]));
        let entry_count = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();
        harness.restore().await.unwrap();
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries(Default::default())
                .await
                .unwrap()
                .len(),
            entry_count
        );
    }

    /// Without a resolver the session's model reference is unresolvable, so
    /// restore keeps the construction-time model.
    #[tokio::test]
    async fn test_restore_without_resolver_keeps_construction_model() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        session
            .storage()
            .append_entry(&SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                provider: "anthropic".into(),
                model_id: "claude-opus".into(),
            })
            .await
            .unwrap();

        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.restore().await.unwrap();
        assert_eq!(harness.model().id, "test");
        assert_eq!(harness.agent().state().model.id, "test");
    }

    /// `set_model` persists a `model_change` entry, and a restore replays it
    /// through the resolver even after the in-memory model was scrambled.
    #[tokio::test]
    async fn test_set_model_persists_and_replays() {
        let session = Session::new(MemStorage::new());
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_model_resolver(test_model_resolver());

        harness.set_model(resolved_model()).await.unwrap();
        assert_eq!(harness.model().id, "claude-opus");
        assert_eq!(harness.agent().state().model.id, "claude-opus");

        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        match entries.last() {
            Some(SessionTreeEntry::ModelChange {
                provider, model_id, ..
            }) => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model_id, "claude-opus");
            }
            other => panic!("expected a trailing ModelChange, got {other:?}"),
        }

        // Scramble the in-memory model; restore replays the persisted choice.
        harness.agent_mut().set_model(test_model());
        harness.restore().await.unwrap();
        assert_eq!(harness.model().id, "claude-opus");
        assert_eq!(harness.agent().state().model.id, "claude-opus");
    }

    /// `set_active_tools` rejects names outside the mounted set, filters the
    /// agent's tools, persists an `active_tools_change` entry, and a restore
    /// replays the selection after the in-memory set was scrambled.
    #[tokio::test]
    async fn test_set_active_tools_validates_filters_and_replays() {
        let session = Session::new(MemStorage::new());
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_tools(two_tools());
        assert_eq!(mounted_names(&harness), ["echo", "other"]);

        // An unknown name is refused and persists nothing.
        let before = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();
        let err = harness
            .set_active_tools(vec!["bogus".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err:?}");
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries(Default::default())
                .await
                .unwrap()
                .len(),
            before
        );
        assert_eq!(mounted_names(&harness), ["echo", "other"]);

        harness
            .set_active_tools(vec!["echo".to_string()])
            .await
            .unwrap();
        assert_eq!(mounted_names(&harness), ["echo"]);
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        match entries.last() {
            Some(SessionTreeEntry::ActiveToolsChange {
                active_tool_names, ..
            }) => assert_eq!(active_tool_names, &["echo".to_string()]),
            other => panic!("expected a trailing ActiveToolsChange, got {other:?}"),
        }

        // Scramble the in-memory tool list; restore replays the selection.
        harness.agent_mut().set_tools(two_tools());
        harness.restore().await.unwrap();
        assert_eq!(mounted_names(&harness), ["echo"]);
    }

    #[tokio::test]
    async fn test_persist_failure_on_first_message_reverts_to_empty() {
        let storage = MemStorage::new();
        *storage.fail_at_call.lock().unwrap() = 1;
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        let err = harness.prompt("hi").await.unwrap_err();
        assert!(
            err.to_string().contains("injected append failure"),
            "{err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);

        // Nothing persisted, so the reverted transcript is empty too —
        // identical to what a fresh harness would restore from this session.
        assert!(harness.agent().state().messages.is_empty());
        assert!(
            harness
                .session()
                .build_context_entries()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_persist_failure_mid_turn_keeps_only_the_persisted_prefix() {
        let storage = MemStorage::new();
        *storage.fail_at_call.lock().unwrap() = 2;
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        // The user message persists; the assistant reply does not.
        let err = harness.prompt("hi").await.unwrap_err();
        assert!(
            err.to_string().contains("injected append failure"),
            "{err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);

        // Both views hold exactly the persisted prefix: the pending user
        // message, and nothing else.
        let messages = &harness.agent().state().messages;
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(matches!(&messages[0], AgentMessage::User { .. }));
        assert_eq!(
            harness
                .session()
                .build_context_entries()
                .await
                .unwrap()
                .len(),
            1
        );

        // With the failure spent, continuing answers the pending user
        // message — the conversation continues coherently, not forked.
        let produced = harness.continue_().await.unwrap();
        assert!(
            produced
                .iter()
                .any(|m| matches!(m, AgentMessage::Assistant { .. }))
        );
        assert_eq!(harness.agent().state().messages.len(), 2);
        assert_eq!(
            harness
                .session()
                .build_context_entries()
                .await
                .unwrap()
                .len(),
            2
        );
    }

    /// A `MessageEnd` listener observes the message already persisted: the
    /// harness middleware appends to the session before listeners run, so a
    /// crash at any later point never loses the completed messages.
    #[tokio::test]
    async fn test_message_end_persists_before_listener() {
        use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("session.jsonl")
            .to_string_lossy()
            .into_owned();
        let meta = JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        };
        let storage = JsonlSessionStorage::create(std::path::Path::new(&path), meta)
            .await
            .unwrap();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        let path_in_listener = path.clone();
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let path = path_in_listener.clone();
            Box::pin(async move {
                if matches!(event, AgentEvent::MessageEnd { .. }) {
                    let content = tokio::fs::read_to_string(&path).await.unwrap();
                    let entries = content.lines().filter(|l| !l.trim().is_empty()).count();
                    // Header + every message emitted so far is already on disk.
                    assert!(
                        entries >= 2,
                        "the MessageEnd listener must observe the message persisted"
                    );
                }
            })
        }));

        let _ = harness.prompt("hi").await.unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let entries = content.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(entries, 3, "header + user + assistant");
    }

    /// A crash mid-tool-turn leaves every completed message recoverable: the
    /// user prompt, the tool-use assistant message, and the tool result were
    /// all persisted at their MessageEnd, and a reopen restores exactly them.
    #[tokio::test]
    async fn test_mid_tool_turn_messages_survive_restore() {
        let storage = MemStorage::new();
        // Fail on the second provider call's assistant append (call 4: user,
        // tool-use assistant, tool result, then the final assistant fails).
        *storage.fail_at_call.lock().unwrap() = 4;
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        assert!(harness.prompt("use the tool").await.is_err());

        // The persisted prefix holds the completed part of the tool turn.
        let entries = harness.session().build_context_entries().await.unwrap();
        assert_eq!(entries.len(), 3, "{entries:?}");

        // A fresh harness over the same session restores exactly the prefix.
        let entries_snapshot = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let mut reopened = AgentHarness::new(
            Session::new(MemStorage::from_entries(entries_snapshot)),
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        reopened.restore().await.unwrap();
        let transcript = reopened.agent().state().messages.clone();
        assert_eq!(transcript.len(), 3, "{transcript:?}");
        assert!(matches!(
            &transcript[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::ToolUse),
                ..
            }
        ));
        assert!(matches!(&transcript[2], AgentMessage::ToolResult { .. }));
    }

    /// Split-turn compaction: a cut inside an oversized tool turn summarizes
    /// the history and the turn prefix separately (two summarization calls),
    /// keeps the tool chain in the prefix, and merges the results into one
    /// boundary summary.
    #[tokio::test]
    async fn test_split_turn_compacts_oversized_tool_turn() {
        let tool_use = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({ "path": "x".repeat(500) }),
                thought_signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let tool_result = AgentMessage::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text {
                text: "y".repeat(500),
                signature: None,
            }],
            is_error: false,
            details: None,
            usage: None,
            added_tool_names: None,
            timestamp: chrono::Utc::now(),
        };
        let transcript = vec![
            AgentMessage::user("earlier work"),
            AgentMessage::user("large tool turn"),
            tool_use,
            tool_result,
            scripted_assistant("done".into(), "test", "test"),
        ];

        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![ScriptedTurn::Answer(8)]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.agent_mut().replace_transcript(transcript);
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 20,
            ..Default::default()
        });
        assert!(harness.needs_compaction());

        let result = harness.compact(None).await.unwrap();
        // History + turn prefix: two separate summarization calls.
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            result.summary.contains("**Turn Context (split turn):**"),
            "{}",
            result.summary
        );
        assert!(result.tokens_after < result.tokens_before);
        // The retained tail is the final answer only.
        assert_eq!(result.retained_tail.len(), 1);
        assert!(matches!(
            &result.retained_tail[0],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            }
        ));
    }

    /// navigate_tree moves the cursor to an earlier entry, rebuilds the
    /// transcript from the new path, and appends a branch summary.
    #[tokio::test]
    async fn test_navigate_tree_moves_and_summarizes_branch() {
        // A stream whose summarization calls answer with a fixed summary and
        // record the messages they were fed; normal turns play answers.
        struct NavigateStreamFn {
            summarized: Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl StreamFn for NavigateStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if context.system_prompt == crate::compaction::SUMMARIZATION_SYSTEM_PROMPT {
                    let mut out = Vec::new();
                    for m in &context.messages {
                        if let AgentMessage::User { content, .. } = m {
                            for b in content {
                                if let ContentBlock::Text { text, .. } = b {
                                    out.push(text.clone());
                                }
                            }
                        }
                    }
                    self.summarized.lock().unwrap().extend(out);
                    return Ok(scripted_assistant("branch summary".into(), "test", "test"));
                }
                Ok(scripted_assistant("answer".into(), "test", "test"))
            }
        }

        let summarized = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(NavigateStreamFn {
                summarized: Arc::clone(&summarized),
            }),
        );
        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();
        let full_len = harness.agent().state().messages.len();
        assert_eq!(full_len, 4);

        // The first turn's assistant reply: navigating back to it keeps the
        // whole first turn on the active path.
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let first_reply_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();

        harness
            .navigate_tree_with_options(
                &first_reply_id,
                NavigateTreeOptions {
                    summarize: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // The transcript now holds only the first turn (user + answer) plus
        // the branch summary carrier; the second turn's messages are on the
        // abandoned branch.
        assert_eq!(harness.agent().state().messages.len(), 3);

        // The branch summary summarized the abandoned second turn, not the
        // first turn the navigation kept.
        let captured: Vec<String> = summarized.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "{captured:?}");
        assert!(captured[0].contains("[User]: second"), "{captured:?}");
        assert!(
            !captured[0].contains("[User]: first"),
            "the kept turn must not be re-summarized: {captured:?}"
        );

        // A branch summary entry was appended.
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, SessionTreeEntry::BranchSummary { .. })),
            "{entries:?}"
        );
    }

    /// Plain navigation (the TS default, `summarize: false`) never calls the
    /// model and appends no branch summary — the cursor moves, the transcript
    /// rebuilds, and the result carries the target's editor text for user
    /// targets.
    #[tokio::test]
    async fn test_navigate_tree_default_skips_summarization() {
        struct NoSummaryStream(Arc<std::sync::Mutex<usize>>);
        #[async_trait::async_trait]
        impl StreamFn for NoSummaryStream {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if context.system_prompt == crate::compaction::SUMMARIZATION_SYSTEM_PROMPT {
                    *self.0.lock().unwrap() += 1;
                }
                Ok(scripted_assistant("answer".into(), "test", "test"))
            }
        }

        let summarization_calls = Arc::new(std::sync::Mutex::new(0usize));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(NoSummaryStream(Arc::clone(&summarization_calls))),
        );
        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();

        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let first_reply_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();

        let result = harness.navigate_tree(&first_reply_id).await.unwrap();
        // No model call, no summary entry, no summary carrier message.
        assert_eq!(*summarization_calls.lock().unwrap(), 0);
        assert_eq!(harness.agent().state().messages.len(), 2);
        assert!(result.summary_entry_id.is_none());
        assert!(!result.cancelled);
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, SessionTreeEntry::BranchSummary { .. })),
            "no summary entry without summarize: {entries:?}"
        );

        // Navigating to the first user message reports its text and resets
        // the cursor to the root (its parent).
        let first_user_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::User { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        let result = harness.navigate_tree(&first_user_id).await.unwrap();
        assert_eq!(result.editor_text.as_deref(), Some("first"));
        assert!(harness.session().leaf_id().await.unwrap().is_none());
    }

    /// A navigation label attaches to the summary entry when one is generated
    /// and to the target entry otherwise — TS `appendLabelChange` on either
    /// node.
    #[tokio::test]
    async fn test_navigate_tree_label_attaches_to_summary_or_target() {
        struct SummarizeStreamFn;
        #[async_trait::async_trait]
        impl StreamFn for SummarizeStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if context.system_prompt == crate::compaction::SUMMARIZATION_SYSTEM_PROMPT {
                    return Ok(scripted_assistant("branch summary".into(), "test", "test"));
                }
                Ok(scripted_assistant("answer".into(), "test", "test"))
            }
        }

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(SummarizeStreamFn),
        );
        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();

        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let first_reply_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();

        // With a summary, the label lands on the branch summary entry.
        let result = harness
            .navigate_tree_with_options(
                &first_reply_id,
                NavigateTreeOptions {
                    summarize: true,
                    label: Some("nav-summary".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let summary_id = result.summary_entry_id.expect("summary entry");
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::Label { target_id, label, .. }
                    if target_id == &summary_id && label.as_deref() == Some("nav-summary")
            )),
            "label must attach to the summary entry: {entries:?}"
        );

        // Without a summary, the label lands on the target entry.
        let first_user_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::User { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        harness
            .navigate_tree_with_options(
                &first_user_id,
                NavigateTreeOptions {
                    summarize: false,
                    label: Some("nav-target".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::Label { target_id, label, .. }
                    if target_id == &first_user_id && label.as_deref() == Some("nav-target")
            )),
            "label must attach to the target entry without a summary: {entries:?}"
        );
    }

    /// An aborted summarization cancels the navigation before any cursor
    /// move or entry append (TS `{ cancelled: true, aborted: true }`); a
    /// `session_before_tree` hook cancellation behaves the same way.
    #[tokio::test]
    async fn test_navigate_tree_abort_and_hook_cancel_leave_tree_untouched() {
        struct AbortSummaryStreamFn;
        #[async_trait::async_trait]
        impl StreamFn for AbortSummaryStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if context.system_prompt == crate::compaction::SUMMARIZATION_SYSTEM_PROMPT {
                    let mut m = scripted_assistant("".into(), "test", "test");
                    if let AgentMessage::Assistant { stop_reason, .. } = &mut m {
                        *stop_reason = Some(StopReason::Aborted);
                    }
                    return Ok(m);
                }
                Ok(scripted_assistant("answer".into(), "test", "test"))
            }
        }

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(AbortSummaryStreamFn),
        );
        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();
        let leaf_before = harness.session().leaf_id().await.unwrap();
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let first_reply_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();

        let result = harness
            .navigate_tree_with_options(
                &first_reply_id,
                NavigateTreeOptions {
                    summarize: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(result.cancelled && result.aborted);
        assert_eq!(harness.session().leaf_id().await.unwrap(), leaf_before);
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, SessionTreeEntry::BranchSummary { .. })),
            "aborted navigation must not append a summary: {entries:?}"
        );

        // A hook cancellation returns `cancelled` without moving the cursor.
        harness.on(
            HookPoint::SessionBeforeTree,
            Arc::new(|ctx: HookContext| ctx.with_cancel_tree()),
        );
        let result = harness
            .navigate_tree_with_options(
                &first_reply_id,
                NavigateTreeOptions {
                    summarize: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(result.cancelled && !result.aborted);
        assert_eq!(harness.session().leaf_id().await.unwrap(), leaf_before);
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, SessionTreeEntry::Label { .. })),
            "cancelled navigation must not append anything: {entries:?}"
        );
    }

    /// `skill_with_instructions` appends the additional instructions to the
    /// skill block before running it.
    #[tokio::test]
    async fn test_skill_with_instructions_appends_to_the_block() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_resources(HarnessResources {
            skills: vec![Skill {
                name: "review".into(),
                description: String::new(),
                location: "/proj/skills/review.md".into(),
                content: "Check the diff.".into(),
            }],
            ..Default::default()
        });

        harness
            .skill_with_instructions("review", "Focus on errors.")
            .await
            .unwrap();
        let transcript = &harness.agent().state().messages;
        let prompt = transcript
            .iter()
            .find_map(|m| match m {
                AgentMessage::User { content, .. } => match &content[0] {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        assert!(prompt.contains("Check the diff."));
        assert!(prompt.ends_with("\n\nFocus on errors."), "{prompt:?}");
    }

    /// `append_message` persists immediately when idle and lands in the
    /// mutation queue mid-run, flushed at the next turn boundary.
    #[tokio::test]
    async fn test_append_message_idle_immediate_running_queued() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        // Idle: appended straight to the session.
        harness
            .append_message(AgentMessage::user("appended"))
            .await
            .unwrap();
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::Message {
                    message: AgentMessage::User { content, .. },
                    ..
                } if matches!(&content[0], ContentBlock::Text { text, .. } if text == "appended")
            )),
            "idle append persists immediately"
        );

        // Mid-run: queued as a mutation, flushed once the run settles.
        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    // Mid-run, from inside the turn: the harness method needs
                    // `&mut`, so the handle is the only way in.
                    handle.append_message(AgentMessage::BashExecution {
                        command: "echo mid-run".into(),
                        output: "mid-run".into(),
                        exit_code: Some(0),
                        cancelled: false,
                        truncated: false,
                        full_output_path: None,
                        exclude_from_context: None,
                        timestamp: chrono::Utc::now(),
                    });
                }
            })
        }));
        let _ = harness.prompt("turn").await.unwrap();

        // The queued execution is durable once the run settles.
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::Message {
                    message: AgentMessage::BashExecution { command, .. },
                    ..
                } if command == "echo mid-run"
            )),
            "the mid-run append flushed to the session"
        );

        // And it lands after the turn's own tool results, not between a tool
        // call and its result — a split turn would be rejected by the provider.
        let messages = harness.agent().state().messages.clone();
        let bash_at = messages
            .iter()
            .position(|m| matches!(m, AgentMessage::BashExecution { .. }))
            .expect("the execution is in the live transcript");
        let last_result = messages
            .iter()
            .rposition(|m| matches!(m, AgentMessage::ToolResult { .. }));
        if let Some(last_result) = last_result {
            assert!(
                bash_at > last_result,
                "execution at {bash_at} must follow the turn's last tool result at {last_result}"
            );
        }
    }

    /// `message_entry_ids` is indexed by `compact()` to resolve the real
    /// `first_kept_entry_id`, so it must stay index-aligned with the agent
    /// transcript through every path that grows the transcript — including the
    /// three that append a message no `MessageEnd` produced.
    #[tokio::test]
    async fn appended_messages_keep_the_entry_id_index_aligned() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        // Idle path.
        harness
            .append_message(AgentMessage::user("idle aside"))
            .await
            .unwrap();
        assert_eq!(
            harness.agent().state().messages.len(),
            harness.message_entry_ids.len(),
            "idle append stays aligned"
        );

        harness.prompt("one").await.unwrap();
        assert_eq!(
            harness.agent().state().messages.len(),
            harness.message_entry_ids.len(),
            "a plain turn stays aligned"
        );

        // Mid-run path: queued from inside the turn, flushed at the boundary.
        let handle = harness.handle();
        let queued = handle.clone();
        let _sub = harness.agent().subscribe(Arc::new(move |event, _t| {
            let handle = queued.clone();
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. }) {
                    handle.append_message(AgentMessage::user("mid-run aside"));
                }
            })
        }));
        harness.prompt("two").await.unwrap();

        let transcript = harness.agent().state().messages.clone();
        assert_eq!(
            transcript.len(),
            harness.message_entry_ids.len(),
            "a mid-run append stays aligned"
        );
        // The recorded id is the real entry, not a synthetic gap: a compaction
        // cutting here must find an anchor to walk the tree from.
        let aside = transcript
            .iter()
            .position(|m| matches!(m, AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "mid-run aside")))
            .expect("the aside reached the transcript");
        let anchor = harness.message_entry_ids[aside]
            .as_deref()
            .expect("the aside carries its entry id");
        assert!(
            harness
                .session()
                .storage()
                .get_entry(anchor)
                .await
                .unwrap()
                .is_some(),
            "the recorded id names a real session entry"
        );
    }

    /// Duplicate tool names are refused by `set_tools`; unknown active tools
    /// are refused by `set_active_tools`.
    #[tokio::test]
    async fn test_set_tools_validates_names() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>,
            Arc::new(NamedTool("other")) as Arc<dyn crate::tool::AgentTool>,
        ]));

        let dup = Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>,
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>,
        ]);
        let err = harness.set_tools(dup).unwrap_err();
        assert!(err.to_string().contains("duplicate tool name"), "{err}");
        assert_eq!(harness.tools().len(), 2);

        let err = harness
            .set_active_tools(vec!["no-such-tool".into()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "{err}");
    }

    /// Queue modes switch between All (drain everything) and OneAtATime
    /// (drain one message per turn).
    #[tokio::test]
    async fn test_queue_modes_switch_between_all_and_one_at_a_time() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        assert_eq!(harness.steering_mode(), crate::agent::QueueMode::OneAtATime);
        harness.set_steering_mode(crate::agent::QueueMode::All);
        harness.set_follow_up_mode(crate::agent::QueueMode::All);
        assert_eq!(harness.steering_mode(), crate::agent::QueueMode::All);
        assert_eq!(harness.follow_up_mode(), crate::agent::QueueMode::All);
    }

    /// Shutdown clears every queue, cancels work, and refuses further
    /// operations with a typed error; queue/settled events fire.
    #[tokio::test]
    async fn test_shutdown_clears_queues_and_refuses_operations() {
        let events: Arc<std::sync::Mutex<Vec<HarnessEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_slot = Arc::clone(&events);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let _sub = harness.subscribe_harness(Arc::new(move |e| {
            events_slot.lock().unwrap().push(e);
        }));
        harness.next_turn("queued", Vec::new());
        assert!(harness.has_next_turn());

        harness.request_shutdown();
        assert!(harness.is_shutdown());
        assert!(!harness.has_next_turn(), "next-turn queue cleared");
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, HarnessEvent::QueueUpdate { next_turn: 1, .. })),
            "queue update fired on enqueue: {:?}",
            events.lock().unwrap()
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, HarnessEvent::QueueUpdate { next_turn: 0, .. })),
            "queue update fired on shutdown clear: {:?}",
            events.lock().unwrap()
        );

        let err = harness.prompt("refused").await.unwrap_err();
        assert!(err.to_string().contains("shut down"), "{err}");
        let err = harness
            .append_message(AgentMessage::user("nope"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("shut down"), "{err}");
    }

    #[tokio::test]
    async fn abort_reports_the_queued_messages_it_discarded() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let slot = Arc::clone(&events);
        let _sub = harness.subscribe_harness(Arc::new(move |e| slot.lock().unwrap().push(e)));

        let handle = harness.handle();
        handle.steer(AgentMessage::user("steer one"));
        handle.steer(AgentMessage::user("steer two"));
        handle.follow_up(AgentMessage::user("follow one"));
        handle.abort();

        let events = events.lock().unwrap();
        let aborted = events
            .iter()
            .find_map(|e| match e {
                HarnessEvent::Abort {
                    cleared_steer,
                    cleared_follow_up,
                } => Some((cleared_steer, cleared_follow_up)),
                _ => None,
            })
            .expect("abort emits its cleared queues");
        // Every undelivered message is handed back, so a consumer can restore
        // the user's input rather than silently losing it.
        assert_eq!(aborted.0.len(), 2, "{:?}", aborted.0);
        assert_eq!(aborted.1.len(), 1, "{:?}", aborted.1);
        assert!(!harness.agent().has_queued_messages(), "queues emptied");
    }

    #[tokio::test]
    async fn save_point_reports_whether_mutations_were_pending() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let slot = Arc::clone(&events);
        let _sub = harness.subscribe_harness(Arc::new(move |e| slot.lock().unwrap().push(e)));

        harness.prompt("clean turn").await.unwrap();
        let clean: Vec<bool> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::SavePoint {
                    had_pending_mutations,
                } => Some(*had_pending_mutations),
                _ => None,
            })
            .collect();
        assert!(!clean.is_empty(), "a settled turn reaches a save point");
        assert!(
            clean.iter().all(|p| !p),
            "nothing was queued, so nothing was pending: {clean:?}"
        );

        // A mutation queued mid-run is pending at the next boundary.
        events.lock().unwrap().clear();
        let handle = harness.handle();
        handle.append_message(AgentMessage::user("queued aside"));
        harness.prompt("second turn").await.unwrap();
        let with_pending: Vec<bool> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::SavePoint {
                    had_pending_mutations,
                } => Some(*had_pending_mutations),
                _ => None,
            })
            .collect();
        assert!(
            with_pending.iter().any(|p| *p),
            "the queued mutation is reported as pending: {with_pending:?}"
        );
    }

    #[tokio::test]
    async fn dropping_a_harness_subscription_stops_delivery() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let slot = Arc::clone(&events);
        let sub = harness.subscribe_harness(Arc::new(move |e| slot.lock().unwrap().push(e)));

        harness.next_turn("first", Vec::new());
        let delivered = events.lock().unwrap().len();
        assert!(delivered > 0, "the live subscription receives events");

        drop(sub);
        harness.next_turn("second", Vec::new());
        assert_eq!(
            events.lock().unwrap().len(),
            delivered,
            "a dropped subscription receives nothing further"
        );
    }

    /// Shutdown drops queued (unpersisted) mutations: a mid-run model
    /// change queued on the mutation queue must not be flushed after
    /// shutdown.
    #[tokio::test]
    async fn test_shutdown_drops_pending_mutations() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    // Queue a model change mid-run; shutdown must drop it.
                    handle.set_model(resolved_model());
                    handle.request_shutdown();
                }
            })
        }));

        let _ = harness.prompt("use the tool").await;
        // Shutdown cleared the mutation queue: no model_change was persisted.
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, SessionTreeEntry::ModelChange { .. })),
            "shutdown must not flush queued mutations: {entries:?}"
        );
    }

    /// A prompt batch with image content produces a user message carrying
    /// both blocks; skills and templates expand into prompts; next_turn
    /// reflects queued runtime mutations.
    #[tokio::test]
    async fn test_prompt_input_skills_and_templates() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_resources(HarnessResources {
            skills: vec![Skill {
                name: "summarize".into(),
                description: "summarize the work".into(),
                location: "/proj/skills/summarize.md".into(),
                content: "Summarize everything.".into(),
            }],
            prompt_templates: vec![PromptTemplate {
                name: "review".into(),
                content: "Review $1 for bugs.".into(),
            }],
            ..Default::default()
        });

        // Image content joins the user message.
        let messages = harness
            .prompt_input(PromptInput {
                text: "describe this".into(),
                images: vec![ContentBlock::Image {
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                }],
                asides: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            &messages[0],
            AgentMessage::User { content, .. }
                if content.len() == 2
                    && matches!(&content[1], ContentBlock::Image { .. })
        ));

        // Skills and templates expand into a prompt turn.
        harness.skill("summarize").await.unwrap();
        harness
            .prompt_from_template("review", &["main.rs".into()])
            .await
            .unwrap();
        let transcript = &harness.agent().state().messages;
        assert!(transcript.iter().any(|m| matches!(
            m,
            AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "Review main.rs for bugs.")
        )));
        // The skill invocation carries the TS `<skill name location>` block.
        harness.skill("summarize").await.unwrap();
        let transcript = &harness.agent().state().messages;
        assert!(transcript.iter().any(|m| matches!(
            m,
            AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text.contains("<skill name=\"summarize\" location=\"/proj/skills/summarize.md\">"))
        )));

        // The harness's next-turn queue runs BEFORE the prompt's own
        // message (pi-agent-core semantics, TS agent-harness executeTurn).
        harness.next_turn("queued next", Vec::new());
        assert!(harness.has_next_turn());
        let messages = harness.prompt("direct").await.unwrap();
        assert!(!harness.has_next_turn());
        assert!(matches!(
            &messages[0],
            AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "queued next")
        ));
        assert!(matches!(
            &messages[1],
            AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "direct")
        ));
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A mid-run active-tools change narrows the next provider request's
    /// context and persists an active_tools_change entry.
    #[tokio::test]
    async fn test_handle_set_active_tools_mid_run() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>,
            Arc::new(NamedTool("other")) as Arc<dyn crate::tool::AgentTool>,
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_active_tools(vec!["echo".to_string()]);
                }
            })
        }));

        let _ = harness.prompt("use the tool").await.unwrap();
        // The queued selection was applied and persisted.
        assert_eq!(
            harness.active_tool_names().map(|n| n.to_vec()),
            Some(vec!["echo".to_string()])
        );
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ActiveToolsChange { active_tool_names, .. }
                if active_tool_names == &["echo".to_string()]
        )));
    }

    /// A TurnEnd listener queues a next-turn message mid-run; the next prompt
    /// consumes it before its own message (TS `nextTurn` from a settled
    /// event, delivered at the next `prompt`).
    #[tokio::test]
    async fn test_next_turn_queued_mid_run_consumed_next_prompt() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.next_turn("queued mid-run", Vec::new());
                }
            })
        }));

        let _ = harness.prompt("first").await.unwrap();
        assert!(harness.has_next_turn());

        let messages = harness.prompt("second").await.unwrap();
        assert!(!harness.has_next_turn());
        assert!(matches!(
            &messages[0],
            AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "queued mid-run")
        ));
        assert!(matches!(
            &messages[1],
            AgentMessage::User { content, .. }
                if matches!(&content[0], ContentBlock::Text { text, .. } if text == "second")
        ));
    }

    /// A TurnEnd model switch is persisted before the next provider request:
    /// the model_change entry lands ahead of the messages the new model
    /// produces (TS flushPendingSessionWrites at the turn boundary).
    #[tokio::test]
    async fn test_model_change_precedes_next_turn_messages() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(resolved_model());
                }
            })
        }));

        let _ = harness.prompt("use the tool").await.unwrap();
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        // model_change sits before the new model's assistant reply.
        let model_change_at = entries
            .iter()
            .position(|e| matches!(e, SessionTreeEntry::ModelChange { .. }))
            .expect("model change persisted");
        let last_assistant_at = entries
            .iter()
            .rposition(|e| {
                matches!(
                    e,
                    SessionTreeEntry::Message {
                        message: AgentMessage::Assistant { .. },
                        ..
                    }
                )
            })
            .expect("turn-two assistant");
        assert!(
            model_change_at < last_assistant_at,
            "model change must precede the new model's messages: {entries:?}"
        );
    }

    /// A failed mutation append aborts the run at the turn boundary: the next
    /// provider request never starts.
    #[tokio::test]
    async fn test_mutation_append_failure_blocks_next_request() {
        let storage = MemStorage::new();
        *storage.fail_model_id.lock().unwrap() = Some("claude-opus".into());
        let session = Session::new(storage);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_in_stream = Arc::clone(&calls);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: calls_in_stream,
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let handle = harness.handle();
        let handle_in_listener = handle.clone();
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let turns = Arc::clone(&turn_ends);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = handle_in_listener.clone();
            let turns = Arc::clone(&turns);
            Box::pin(async move {
                if matches!(event, AgentEvent::TurnEnd { .. })
                    && turns.fetch_add(1, AtomicOrdering::SeqCst) == 0
                {
                    handle.set_model(resolved_model());
                }
            })
        }));

        // The flush failure at the turn boundary aborts the run; the second
        // provider call never happens.
        let err = harness.prompt("use the tool").await.unwrap_err();
        assert!(
            err.to_string().contains("injected model change failure"),
            "{err:#}"
        );
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "only the first turn ran"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    #[tokio::test]
    async fn test_harness_hooks() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        use std::sync::atomic::{AtomicBool, Ordering};
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);

        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(move |ctx| {
                called_clone.store(true, Ordering::SeqCst);
                ctx
            }),
        );

        let _ = harness.prompt("Hello").await;
        assert!(called.load(Ordering::SeqCst));
    }

    // A tool that echoes its `message` arg, used to exercise the tool hook
    // path end-to-end through the harness's real ToolContext.
    struct EchoTool;

    #[async_trait::async_trait]
    impl crate::tool::AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes the input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"message": {"type": "string"}}})
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn crate::tool::ToolContext,
        ) -> Result<crate::tool::AgentToolResult, crate::tool::ToolError> {
            let msg = params["message"].as_str().unwrap_or("no message");
            Ok(crate::tool::AgentToolResult::text(msg))
        }
    }

    // A tool whose only distinguishing feature is its name, for exercising
    // the active-tool filter.
    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl crate::tool::AgentTool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "named"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn crate::tool::ToolContext,
        ) -> Result<crate::tool::AgentToolResult, crate::tool::ToolError> {
            Ok(crate::tool::AgentToolResult::text("ok"))
        }
    }

    /// The two-tool mounted set used by active-tool tests: `echo` plus a
    /// second name to filter away.
    fn two_tools() -> Arc<[Arc<dyn crate::tool::AgentTool>]> {
        Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>,
            Arc::new(NamedTool("other")),
        ])
    }

    fn mounted_names(harness: &AgentHarness<MemStorage>) -> Vec<String> {
        harness
            .agent()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    // A stream fn that issues one tool call then stops, so a single prompt
    // drives a full tool-execution round through the harness.
    /// A plain-answer stream that records the model id of every provider
    /// call — the stand-in for a second provider runtime in resolver tests.
    struct TaggedAnswerStreamFn {
        served: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl StreamFn for TaggedAnswerStreamFn {
        async fn stream(
            &self,
            context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            self.served.lock().unwrap().push(context.model.id.clone());
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "provider b".into(),
                    signature: None,
                }],
                model: context.model.id.clone(),
                provider: context.model.provider.clone(),
                api: context.model.api.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Stop),
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    struct ToolUseStreamFn {
        call: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Records the model id of every provider call, when set.
        seen: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl StreamFn for ToolUseStreamFn {
        async fn stream(
            &self,
            context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            if let Some(seen) = &self.seen {
                seen.lock().unwrap().push(context.model.id.clone());
            }
            let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Ok(AgentMessage::Assistant {
                    content: vec![crate::types::ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"message": "hi"}),
                        thought_signature: None,
                    }],
                    model: "test".into(),
                    provider: "test".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            } else {
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                        signature: None,
                    }],
                    model: "test".into(),
                    provider: "test".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Stop),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            }
        }
    }

    #[tokio::test]
    async fn before_provider_request_hook_fires_on_prompt() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        harness.on(
            HookPoint::BeforeProviderRequest,
            Arc::new(move |ctx| {
                count_clone.fetch_add(1, Ordering::SeqCst);
                assert!(ctx.agent_context.is_some());
                ctx
            }),
        );

        let _ = harness.prompt("Hello").await.unwrap();
        assert!(
            count.load(Ordering::SeqCst) >= 1,
            "BeforeProviderRequest must fire when the provider is called"
        );
    }

    #[tokio::test]
    async fn tool_call_and_tool_result_hooks_fire_during_execution() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        use std::sync::atomic::{AtomicUsize, Ordering};
        let tool_call = Arc::new(AtomicUsize::new(0));
        let tool_result = Arc::new(AtomicUsize::new(0));
        let tc = Arc::clone(&tool_call);
        let tr = Arc::clone(&tool_result);
        harness.on(
            HookPoint::ToolCall,
            Arc::new(move |ctx| {
                if ctx.data.get("tool_name").and_then(|v| v.as_str()) == Some("echo") {
                    tc.fetch_add(1, Ordering::SeqCst);
                }
                ctx
            }),
        );
        harness.on(
            HookPoint::ToolResult,
            Arc::new(move |ctx| {
                tr.fetch_add(1, Ordering::SeqCst);
                let _ = ctx;
                ctx
            }),
        );

        let _ = harness.prompt("run the echo tool").await.unwrap();
        assert_eq!(
            tool_call.load(Ordering::SeqCst),
            1,
            "ToolCall hook must fire once for the echo call"
        );
        assert_eq!(
            tool_result.load(Ordering::SeqCst),
            1,
            "ToolResult hook must fire once for the echo result"
        );
    }

    #[tokio::test]
    async fn before_provider_request_mutation_reaches_the_provider() {
        // A stream fn that records the context it actually received.
        let received: Arc<std::sync::Mutex<Vec<AgentMessage>>> = Arc::default();
        let captured = Arc::clone(&received);
        struct CaptureStreamFn {
            captured: Arc<std::sync::Mutex<Vec<AgentMessage>>>,
        }
        #[async_trait::async_trait]
        impl StreamFn for CaptureStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                *self.captured.lock().unwrap() = context.messages.clone();
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "ok".into(),
                        signature: None,
                    }],
                    model: "test".into(),
                    provider: "test".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Stop),
                    usage: Box::new(Usage::default()),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            }
        }

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CaptureStreamFn { captured }),
        );

        // The hook appends a sentinel user message to the context the
        // provider is about to see.
        harness.on(
            HookPoint::BeforeProviderRequest,
            Arc::new(|mut ctx: HookContext| {
                let mut ac = ctx.agent_context.take().expect("context present");
                ac.messages.push(AgentMessage::user("SENTINEL_FROM_HOOK"));
                ctx.agent_context = Some(ac);
                ctx
            }),
        );

        let _ = harness.prompt("hello").await.unwrap();
        let received = received.lock().unwrap();
        assert!(
            received
                .iter()
                .any(|m| matches!(m, AgentMessage::User { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "SENTINEL_FROM_HOOK")))),
            "a mutated context from BeforeProviderRequest must reach the provider"
        );
    }

    #[tokio::test]
    async fn tool_call_hook_can_block_execution() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        // Block every tool call with a reason.
        harness.on(
            HookPoint::ToolCall,
            Arc::new(|ctx: HookContext| ctx.with_block_reason("denied by test hook")),
        );

        let messages = harness.prompt("run the echo tool").await.unwrap();
        // The blocked call surfaces as an error tool result carrying the reason.
        let blocked = messages.iter().any(|m| matches!(m, AgentMessage::ToolResult { is_error, content, .. } if *is_error && content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("denied by test hook")))));
        assert!(
            blocked,
            "a ToolCall hook returning a block reason must abort the call"
        );
    }

    #[tokio::test]
    async fn tool_result_hook_can_patch_the_result() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                seen: None,
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        // Replace whatever the tool produced with a fixed patched payload.
        harness.on(
            HookPoint::ToolResult,
            Arc::new(|ctx: HookContext| {
                ctx.with_tool_result(crate::tool::AgentToolResult::text("PATCHED_BY_HOOK"))
            }),
        );

        let messages = harness.prompt("run the echo tool").await.unwrap();
        let patched = messages.iter().any(|m| matches!(m, AgentMessage::ToolResult { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "PATCHED_BY_HOOK"))));
        assert!(
            patched,
            "a ToolResult hook returning a replacement must patch the result"
        );
    }

    /// A stream that records the system prompt of every context it sees, so
    /// tests can tell which prompt each provider call carried.
    struct SystemPromptSpy {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl StreamFn for SystemPromptSpy {
        async fn stream(
            &self,
            context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            self.seen
                .lock()
                .unwrap()
                .push(context.system_prompt.clone());
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "spy response".into(),
                    signature: None,
                }],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Stop),
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    #[tokio::test]
    async fn before_agent_start_messages_join_the_prompt_batch() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(|ctx: HookContext| {
                ctx.with_inject_messages(vec![AgentMessage::user("INJECTED_BY_HOOK")])
            }),
        );

        let messages = harness.prompt("Hello").await.unwrap();
        let positions: Vec<&str> = messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::User { content, .. } => content.iter().find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            positions,
            ["Hello", "INJECTED_BY_HOOK"],
            "hook messages are appended after the user message, before the response"
        );

        // Injected messages persist like any prompt message.
        let entries = harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let persisted: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Message {
                    message: AgentMessage::User { content, .. },
                    ..
                } => content.iter().find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(persisted, ["Hello", "INJECTED_BY_HOOK"]);
    }

    #[tokio::test]
    async fn before_agent_start_system_prompt_reaches_the_first_context_only() {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "base prompt",
            test_model(),
            Arc::new(SystemPromptSpy {
                seen: Arc::clone(&seen),
            }),
        );

        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(|ctx: HookContext| ctx.with_system_prompt("override prompt")),
        );

        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();

        // The hook fires on every prompt, so both runs see the override in
        // their initial context...
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["override prompt", "override prompt"]
        );
        // ...but the agent state is untouched between runs: an override never
        // becomes the configured prompt.
        assert_eq!(harness.agent().state().system_prompt, "base prompt");
    }

    #[tokio::test]
    async fn before_agent_start_without_override_keeps_the_original_prompt() {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "base prompt",
            test_model(),
            Arc::new(SystemPromptSpy {
                seen: Arc::clone(&seen),
            }),
        );

        // A handler that observes the event but returns no result fields.
        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(|ctx: HookContext| ctx),
        );

        let messages = harness.prompt("Hello").await.unwrap();
        assert_eq!(messages.len(), 2, "user message plus the response");
        assert_eq!(seen.lock().unwrap().as_slice(), ["base prompt"]);
    }
}
