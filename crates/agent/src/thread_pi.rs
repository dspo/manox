// The pi-backed `Thread` facade (built with `feature = "harness-pi"`).
//
// A gpui-free thread owned behind a `ThreadHandle` (`Arc<ThreadCore>`): the
// state lives in a lock, and run events from the tokio actor around a pi
// `AgentSession` (via the `ThreadEngine` contract) flow back through a
// channel, are adapted into `ThreadEvent`s (see `pi_engine::adapt`), and
// broadcast to the handle's subscribers. History is exposed as a display
// sequence (messages interleaved with persisted UI annotation cards) so the
// rebuild path (`ConversationState::rebuild_from_display`) replays it in
// order.
//
// The public surface mirrors the manox `Thread`'s — the workspace compiles
// against one shape. manox-only affordances (pin/archive/notes/goal/team/
// worktree) are inert here; capabilities that need real wiring carry a stub
// with the reason. Approval mode is live: the facade records it, the
// engine's gate enforces it, and the sidecar persists it.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::background_task::TaskSnapshot;
use crate::db::{HistoryEntry, UiNoteRecord};
use crate::goal::ThreadGoal;
use crate::goal_tools::GoalBridge;
use crate::language::Language;
use crate::language_model::{MessageContent, ReasoningEffort, Role, StopReason, TokenUsage};
use pi::types::Model as PiModel;
use crate::message::{Message, MessageUiMetadata};
use crate::thread_engine::{BackendNotice, ReadyInfo, SpawnedEngine, ThreadEngine};

/// Stable `Thread` id used for persistence.
#[derive(Debug, Clone, Default)]
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

/// File-effect policy for confined bash and the fs write fence (the per-call
/// sandbox mode). Defined in the extension layer (`pi_extensions::sandbox`) so
/// the bash tool and the host fence share one vocabulary; re-exported here for
/// the host's session/persistence (wire field `approval_mode`, kebab values).
pub use pi_extensions::sandbox::PermissionMode;

/// History-loading state of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryPhase {
    /// No history pending (fresh / landing threads); the message list is
    /// final as soon as it exists.
    #[default]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Carries the child's [`ThreadId`] (value type) — the event crosses the
    /// kernel boundary, so no handle/entity rides it.
    SubagentStarted {
        id: String,
        subagent_type: String,
        description: String,
        child: ThreadId,
    },
    /// A spawned sub-agent's aggregated progress.
    SubagentProgress {
        id: String,
        subagent_type: String,
        tool_uses: u32,
        token_usage: TokenUsage,
        latest_activity: Option<String>,
        status: ToolCallStatus,
        /// The watchdog's one-line health verdict (`working`, `tool: Bash
        /// 3m12s`, `stalled 2m0s`, `looping: Read src/a.rs`) when the event
        /// carries one; `None` for lifecycle-only progress events.
        health: Option<String>,
    },
    /// A streamed child-session event from a running sub-agent (the pi
    /// bridge of the child's text/thinking deltas and tool lifecycle). The
    /// conversation attaches these to the Agent tool call's drill-down
    /// output; the rail uses them for live activity.
    SubagentChild {
        id: String,
        child: SubagentChildEvent,
    },
    /// Interactive question round trip: `AskUserQuestion` (and bubbled team
    /// member authorizations) parks here. The workspace renders the question
    /// card and answers through [`Thread::respond_authorization`]. Permission
    /// denials never ride this channel — they return a tool error directly.
    ToolCallAuthorization {
        id: String,
        tool_name: String,
        summary: String,
        input: serde_json::Value,
    },
    /// Permission mode changed.
    PermissionModeChanged { mode: PermissionMode },
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
    GoalChanged {
        goal: Option<crate::goal::ThreadGoal>,
    },
    /// The session's effective working directory moved (per-call cwd
    /// resolution advanced the sticky cwd; durable as a `cwd_change`).
    CwdChanged { path: String },
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
    /// The thread's active opt-in browser tool suites changed; the composer
    /// chips are derived state of this mirror.
    BrowserSuitesChanged {
        suites: Vec<crate::pi_engine::BrowserSuite>,
    },
    /// A peer message was delivered from another team member.
    PeerMessage { from: String, content: String },
    /// A queued steer follow-up was drained into `messages`.
    SteerInjected { message_id: String },
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
    permission_mode: PermissionMode,
    messages: Vec<Message>,
    reasoning_effort: ReasoningEffort,
    pinned: bool,
    archived: bool,
    running: bool,
    restored: bool,
    display: Vec<HistoryEntry>,
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
    /// Whether the user explicitly set the permission mode on a landing
    /// thread; the mode is then not overwritten by the session sidecar's
    /// default when the engine materializes.
    permission_mode_explicitly_set: bool,
    /// Whether the user explicitly set the reasoning effort on a landing
    /// thread; the effort is then not overwritten by the session sidecar's
    /// default when the engine materializes.
    reasoning_effort_explicitly_set: bool,
    /// Opt-in browser tool suites the user activated; the composer chips
    /// derive from this mirror. Replayed to the engine when a landing thread
    /// materializes it, so a pre-engine toggle is never dropped.
    browser_suites: Vec<crate::pi_engine::BrowserSuite>,
    /// Whether a suite was toggled since construction; until then the
    /// `Ready` projection seeds the mirror (same pattern as
    /// `permission_mode_explicitly_set`).
    browser_suites_explicitly_set: bool,
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

    /// Shared goal state with the engine's goal tools; `None` only when the
    /// threads db is unavailable (goal features degrade off).
    goal_bridge: Option<Arc<GoalBridge>>,    cwd_path: Option<String>,
    /// Events buffered by cx-free mutations, drained and broadcast to the
    /// handle's subscribers after each operation (replaces `cx.emit`).
    pending_events: Vec<ThreadEvent>,
    /// Engine notice channel parked by `ensure_engine` (lazy engine). `open`
    /// drains its engine directly at construction (no outer `with_mut` to
    /// borrow); `ensure_engine` runs inside a `with_mut`, so it parks the
    /// receiver here and that enclosing call spawns the pump.
    pending_engine_events: Option<tokio::sync::mpsc::UnboundedReceiver<BackendNotice>>,
}

/// A live-thread registry seam: the kernel looks up / registers live threads
/// for team/bus routing through this, never touching the gpui thread_store
/// global directly. The gpui layer wires an impl backed by the thread store.
///
/// **Retention contract** — the registry stores only a **weak** reference per
/// registered thread (`Arc::downgrade`), so registering never by itself keeps
/// a thread alive. [`Self::lookup`] upgrades the weak reference (`None` once
/// the thread is dropped); [`Self::refresh`] prunes stale entries;
/// [`Self::unregister`] removes an entry explicitly (dismiss). The **strong**
/// reference that keeps a thread alive is owned by the live-thread holder —
/// the AgentServer, transitionally the gpui workspace — mirroring the original
/// `downgrade()` + `refresh`-pruning GC. A spawned team member therefore stays
/// alive exactly as long as its owner holds it, and is reclaimed once dropped.
pub trait ThreadRegistry: Send + Sync {
    /// Index `handle` under `id`, storing only a weak reference. The caller
    /// retains strong ownership; the registry never keeps the thread alive.
    fn register(&self, id: &str, handle: &ThreadHandle);
    /// Upgrade the weak reference for `id`; `None` if absent or dropped.
    fn lookup(&self, id: &str) -> Option<ThreadHandle>;
    /// Prune entries whose thread has been dropped.
    fn refresh(&self);
    /// Drop the entry for `id` (thread dismissed or finished).
    fn unregister(&self, id: &str);
}

static THREAD_REGISTRY: std::sync::OnceLock<Arc<dyn ThreadRegistry>> = std::sync::OnceLock::new();

/// Register the process-wide live-thread registry (App startup). First wins.
pub fn set_thread_registry(registry: Arc<dyn ThreadRegistry>) {
    if THREAD_REGISTRY.set(registry).is_err() {
        tracing::warn!("thread registry already registered; ignoring re-registration");
    }
}

/// The registered live-thread registry, or `None` in headless contexts.
pub fn thread_registry() -> Option<&'static Arc<dyn ThreadRegistry>> {
    THREAD_REGISTRY.get()
}

/// The gpui-free handle to a thread. Cheap to clone (`Arc`); state lives
/// behind a lock and events broadcast to channel subscribers. This is the
/// kernel-side unit the AgentServer and (transitionally) the frontends hold.
#[derive(Clone)]
pub struct ThreadHandle(Arc<ThreadCore>);

pub struct ThreadCore {
    /// Read-mostly state: the UI reads 25+ fields per render while the engine
    /// pump writes during a turn, so reads share the `RwLock` and only
    /// mutations take the exclusive write lock.
    state: parking_lot::RwLock<Thread>,
    /// Event subscribers. Carries `Arc<ThreadEvent>` — a transitional shell
    /// because `ThreadEvent::Error(anyhow::Error)` (and, until it becomes a
    /// `ThreadId`, `SubagentStarted.child`) are not `Clone`; once those land
    /// the event can derive `Clone` and this `Arc` comes off.
    subscribers: parking_lot::Mutex<Vec<async_channel::Sender<Arc<ThreadEvent>>>>,
}

impl ThreadHandle {
    /// Wrap a freshly built [`Thread`].
    pub fn new(thread: Thread) -> Self {
        Self(Arc::new(ThreadCore {
            state: parking_lot::RwLock::new(thread),
            subscribers: parking_lot::Mutex::new(Vec::new()),
        }))
    }

    /// Downgrade to a weak reference for the live-thread registry index. The
    /// registry never by itself keeps the thread alive; the strong reference
    /// sits with the live-thread holder.
    pub fn downgrade(&self) -> std::sync::Weak<ThreadCore> {
        Arc::downgrade(&self.0)
    }

    /// Re-upgrade a weak reference, if the thread is still alive.
    pub fn upgrade(weak: &std::sync::Weak<ThreadCore>) -> Option<Self> {
        weak.upgrade().map(Self)
    }

    /// Subscribe to this thread's event stream.
    pub fn subscribe(&self) -> async_channel::Receiver<Arc<ThreadEvent>> {
        let (tx, rx) = async_channel::unbounded();
        self.0.subscribers.lock().push(tx);
        rx
    }

    /// Shared-read the state.
    pub fn read<R>(&self, f: impl FnOnce(&Thread) -> R) -> R {
        let state = self.0.state.read();
        f(&state)
    }

    /// Mutate under the write lock, then broadcast the buffered events.
    /// Three-phase: lock -> mutate (collecting `pending_events`) -> unlock ->
    /// emit. The closure must never await.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Thread) -> R) -> R {
        let (r, events, engine_events) = {
            let mut state = self.0.state.write();
            let r = f(&mut state);
            let events = std::mem::take(&mut state.pending_events);
            let engine_events = state.pending_engine_events.take();
            (r, events, engine_events)
        };
        if let Some(rx) = engine_events {
            drain_engine_notices(self.clone(), rx);
        }
        self.broadcast(events);
        r
    }

    fn broadcast(&self, events: Vec<ThreadEvent>) {
        if events.is_empty() {
            return;
        }
        let mut subs = self.0.subscribers.lock();
        // Drop subscribers whose receiver is gone (view unmount); otherwise
        // the list grows without bound on a long-lived thread.
        subs.retain(|tx| !tx.is_closed());
        if subs.is_empty() {
            return;
        }
        for ev in events {
            let ev = Arc::new(ev);
            for tx in subs.iter() {
                let _ = tx.try_send(ev.clone());
            }
        }
    }
}

impl ThreadHandle {
    /// Handle one backend notice. State-mutating arms run under `with_mut` so
    /// the events they buffer broadcast at the call's exit — this closes the
    /// pump→notice→`pending_events`→broadcast chain. Bus / browser /
    /// session-list arms are handled at handle level (registry / capability),
    /// never under the state lock.
    pub fn handle_notice(&self, notice: BackendNotice) {
        match notice {
            BackendNotice::BusRequest { op, responder } => {
                self.handle_bus_request(op, responder);
            }
            BackendNotice::BrowserRequest { op, responder } => {
                self.handle_browser_request(op, responder);
            }
            BackendNotice::SessionListDirty => {
                if let Some(reg) = thread_registry() {
                    reg.refresh();
                }
            }
            other => self.with_mut(|t| t.handle_notice_inner(other)),
        }
    }

    /// Steer-bus member ops: spawn / inject / abort team member threads through
    /// the live-thread registry (never the gpui thread_store global).
    fn handle_bus_request(
        &self,
        op: pi_extensions::steer_bus::BusOp,
        responder: Option<async_channel::Sender<Result<String, String>>>,
    ) {
        use pi_extensions::steer_bus::BusOp;
        let result: Result<String, String> = match op {
            BusOp::SpawnMember { name, prompt } => {
                let member = self.read(|t| t.new_team_member(name.clone()));
                let mid = member.read(|t| t.id.0.clone());
                if let Some(reg) = thread_registry() {
                    // Weak index only; the AgentServer (live-thread owner) holds
                    // the strong reference that keeps the member alive.
                    reg.register(&mid, &member);
                    reg.refresh();
                }
                let ui = crate::MessageUiMetadata {
                    author: Some(crate::team::author_for("captain")),
                    ..Default::default()
                };
                member.with_mut(|t| {
                    t.insert_user_message_with_ui_metadata(prompt, Some(ui));
                    t.run_turn();
                });
                Ok(mid)
            }
            BusOp::InjectMember { thread_id, payload } => {
                let Some(member) = thread_registry().and_then(|r| r.lookup(&thread_id)) else {
                    if let Some(r) = &responder {
                        let _ = r.try_send(Err(format!("member {thread_id} not found")));
                    }
                    return;
                };
                member.with_mut(|t| {
                    t.deliver_peer_messages(vec![crate::team::PeerMessage {
                        from: "captain".into(),
                        content: payload,
                    }]);
                });
                Ok("injected".into())
            }
            BusOp::AbortMember { thread_id } => {
                let Some(member) = thread_registry().and_then(|r| r.lookup(&thread_id)) else {
                    if let Some(r) = &responder {
                        let _ = r.try_send(Err(format!("member {thread_id} not found")));
                    }
                    return;
                };
                member.with_mut(|t| t.cancel());
                Ok("aborted".into())
            }
        };
        if let Some(r) = responder {
            let _ = r.try_send(result);
        }
    }

    /// Browser ops are a frontend capability: hand the op to the registered
    /// provider and relay its reply on the runtime; fail closed when no
    /// provider is registered (headless contexts).
    fn handle_browser_request(
        &self,
        op: crate::thread_engine::BrowserOp,
        responder: async_channel::Sender<Result<crate::thread_engine::BrowserReply, String>>,
    ) {
        let Some(caps) = crate::capability::provider() else {
            let _ = responder.try_send(Err("browser capability not available".to_string()));
            return;
        };
        let session_id = self.read(|t| t.id.0.clone());
        crate::runtime::handle().spawn(async move {
            let result = crate::capability::CURRENT_SESSION
                .scope(Some(session_id), async { caps.browser_op(op).await })
                .await;
            let _ = responder.send(result).await;
        });
    }
}

impl Thread {
    /// The startup landing state: a detached thread with no engine. No
    /// session is loaded at launch — the user picks a conversation from the
    /// sidebar (`open_existing` swaps in its engine) or starts typing
    /// (`run_turn` materializes a fresh engine on first use).
    pub fn landing(cwd: PathBuf) -> ThreadHandle {
        Self::landing_with_id(ThreadId(uuid::Uuid::new_v4().to_string()), cwd)
    }

    /// A landing thread with a caller-chosen id, so the desktop can bind its
    /// AgentServer session to the same id (`CreateSession` uses the session
    /// id as the `ThreadId`).
    pub fn landing_with_id(id: ThreadId, cwd: PathBuf) -> ThreadHandle {
        ThreadHandle::new(Self {
            id,
            cwd,
            project: None,
            model: crate::pi_providers::default_model(),
            permission_mode: PermissionMode::default(),
            messages: Vec::new(),
            reasoning_effort: ReasoningEffort::default(),
            pinned: false,
            archived: false,
            running: false,
            restored: false,
            display: Vec::new(),
            request_usage: HashMap::new(),
            pending_prompts: Vec::new(),
            pending_images: Vec::new(),
            pending_steers: VecDeque::new(),
            last_user_ui: None,
            engine: None,
            history_phase: HistoryPhase::Ready,
            permission_mode_explicitly_set: false,
            reasoning_effort_explicitly_set: false,
            browser_suites: Vec::new(),
            browser_suites_explicitly_set: false,
            plan_mode: false,
            persisted_plan: None,
            label: "lead".into(),
            goal_bridge: None,
            cwd_path: None,
            pending_events: Vec::new(),
            pending_engine_events: None,
        })
    }

    /// A genuinely empty thread (sidebar new-conversation): never restores
    /// the previous session.
    pub fn new_fresh(id: ThreadId, cwd: PathBuf) -> ThreadHandle {
        Self::open(id, cwd, None, None, true)
    }

    /// Construct a thread bound to a project directory: a fresh session with
    /// the project as its cwd in one step (no recreate, no restore), so the
    /// sidebar never sees an orphaned pre-project session file.
    pub fn new_in_project(id: ThreadId, project: PathBuf) -> ThreadHandle {
        Self::open(id, project.clone(), None, Some(project), true)
    }

    /// Construct a thread backed by a specific session file (sidebar open).
    pub fn open_existing(id: ThreadId, cwd: PathBuf, path: PathBuf) -> ThreadHandle {
        Self::open(id, cwd, Some(path), None, false)
    }

    fn open(
        id: ThreadId,
        cwd: PathBuf,
        initial_path: Option<PathBuf>,
        project: Option<PathBuf>,
        fresh: bool,
    ) -> ThreadHandle {
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
            None,
        );

        let handle = ThreadHandle::new(Self {
            id,
            cwd,
            project,
            model,
            permission_mode: PermissionMode::default(),
            messages: Vec::new(),
            reasoning_effort: ReasoningEffort::default(),
            pinned: false,
            archived: false,
            running: false,
            restored: false,
            display: Vec::new(),
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
            permission_mode_explicitly_set: false,
            reasoning_effort_explicitly_set: false,
            browser_suites: Vec::new(),
            browser_suites_explicitly_set: false,
            plan_mode: false,
            persisted_plan: None,
            label: "lead".into(),
            goal_bridge,
            cwd_path: None,
            pending_events: Vec::new(),
            pending_engine_events: None,
        });
        drain_engine_notices(handle.clone(), events);
        handle
    }

    /// Lazily materialize the engine for a landing thread (no engine until
    /// the user acts: a sidebar open swaps the whole thread via
    /// `open_existing` instead; a first prompt or project bind calls this).
    /// Spawns a fresh session (never restores) bound to `project`, wires the
    /// notice drainer, and replays the stored permission mode / reasoning
    /// effort. `spawn_engine` is infallible (it only queues the actor), so
    /// the engine is always available after this returns.
    fn ensure_engine(&mut self, project: Option<PathBuf>) {
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
            None,
        );
        if self.permission_mode != PermissionMode::default() {
            engine.set_permission_mode(self.permission_mode);
        }
        if self.reasoning_effort != ReasoningEffort::default() {
            engine.set_thinking_level(Some(self.reasoning_effort.wire_value().to_string()));
        }
        // Toggles that landed while the thread was engine-less parked in the
        // mirror; the fresh engine replays them before the first prompt.
        for suite in self.browser_suites.clone() {
            engine.set_browser_suite(suite, true);
        }
        self.engine = Some(engine.clone());
        self.pending_engine_events = Some(events);
    }

    /// Handle one backend notice's state-mutating arms. Invoked by
    /// [`ThreadHandle::handle_notice`] under `with_mut`, so the events each
    /// arm buffers broadcast at the call's exit. Bus / browser / session-list
    /// arms live on the handle (registry / capability) and never reach here.
    fn handle_notice_inner(&mut self, notice: BackendNotice) {
        match notice {
            BackendNotice::Event(event) => {
                // Mirror the gate policy before the chip hears about the
                // change.
                if let ThreadEvent::PermissionModeChanged { mode } = *event {
                    self.permission_mode = mode;
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
                    self.running = true;
                }
                if let ThreadEvent::CwdChanged { path } = &*event {
                    self.cwd_path = Some(path.clone());
                }
                self.pending_events.push(*event);
            }
            BackendNotice::LiveHistory => {
                // Mid-run mirror refresh (no UI event): the workspace's
                // switch-back rebuild reads `messages()`, so a thread parked
                // while still generating shows current progress instead of the
                // last settled snapshot. The authoritative `Settled` refresh
                // replaces this mirror.
                self.refresh_history();
            }
            BackendNotice::Ready(notice) => {
                let ReadyInfo {
                    restored,
                    model,
                    permission_mode,
                    reasoning_effort,
                    browser_suites,
                    plan_mode,
                    plan_file,
                    plan_review_pending,
                    plan_snapshot,
                } = *notice;
                // Unconditional: a session switch must drop the previous
                // session's plan when the opened session has none.
                self.persisted_plan = plan_snapshot
                    .and_then(|v| serde_json::from_value(v).ok());
                self.restored = restored;
                if let Some(m) = model {
                    self.model = Some(m);
                }
                if !self.permission_mode_explicitly_set {
                    self.permission_mode = permission_mode;
                }
                if !self.reasoning_effort_explicitly_set {
                    self.reasoning_effort = reasoning_effort;
                }
                // A toggle since construction outranks the projection (the
                // queued `SetBrowserSuite` has not reached the actor yet when
                // this lands, so the projection cannot know about it).
                if !self.browser_suites_explicitly_set && self.browser_suites != browser_suites {
                    self.browser_suites = browser_suites.clone();
                    self.pending_events.push(ThreadEvent::BrowserSuitesChanged {
                        suites: browser_suites,
                    });
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
                    self.pending_events.push(ThreadEvent::PlanReady { plan_file, title });
                }
                let was_loading = self.history_phase.is_loading();
                self.history_phase = HistoryPhase::Ready;
                self.refresh_history();
                // Rebuild on restore, and also after a failed restore — the
                // preview may have streamed a corrupt file's partial content
                // and the fresh fallback session is empty, so the workspace
                // must return to the hero. Fresh threads skip the rebuild
                // (nothing changed since attach).
                if restored || was_loading {
                    self.pending_events.push(ThreadEvent::HistoryRestored);
                }
            }
            BackendNotice::HistoryProgress => {
                if self.history_phase.is_loading() {
                    self.refresh_history();
                    self.pending_events.push(ThreadEvent::HistoryProgress);
                }
            }
            BackendNotice::Settled {
                cancelled,
                failed,
                steered,
                stranded,
            } => {
                for message_id in steered {
                    self.pending_events.push(ThreadEvent::SteerInjected { message_id });
                }
                self.running = false;
                self.pending_steers.clear();
                self.refresh_history();
                self.pending_events.push(ThreadEvent::TurnFinished {
                    cancelled,
                    failed,
                    stranded_steer_ids: stranded,
                });
                // Re-fire: if peer messages arrived during the run
                // (pending_prompts non-empty), start a follow-up turn.
                // Mirrors manox-actor pending_submits drain.
                if !self.pending_prompts.is_empty() {
                    self.run_turn();
                }
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
                self.pending_events.push(ThreadEvent::HistoryRestored);
                self.pending_events.push(ThreadEvent::Error(err));
            }
            BackendNotice::SteerDelivered { from, reason: _, payload } => {
                // Deliver the subagent's final text as a peer message and
                // let a turn fire — the Captain reliably observes the
                // result without polling.
                let sender = match &from {
                    pi_extensions::steer_bus::AgentId::Subagent(addr) => addr.clone(),
                    pi_extensions::steer_bus::AgentId::Captain => "captain".to_string(),
                    pi_extensions::steer_bus::AgentId::User => "user".to_string(),
                };
                self.deliver_peer_messages(vec![crate::team::PeerMessage {
                    from: sender,
                    content: payload.text,
                }]);
            }
            // Bus / browser / session-list arms are dispatched at the handle
            // level (`ThreadHandle::handle_notice`); they never reach here.
            // Listed explicitly (not `_`) so a new `BackendNotice` variant
            // forces a decision here at compile time.
            BackendNotice::BusRequest { .. }
            | BackendNotice::BrowserRequest { .. }
            | BackendNotice::SessionListDirty => {}
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
    fn refresh_history(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        let seq = engine.history();
        self.display = seq.clone();
        let mut mapped: Vec<Message> = seq
            .into_iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Message(message) => Some(message),
                HistoryEntry::Note(_) => None,
            })
            .collect();
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
    }

    // ── Thread duck-type: the turn pipeline ────────────────────────────────

    pub fn insert_user_message_with_ui_metadata(
        &mut self,
        text: String,
        ui: Option<MessageUiMetadata>,
    ) {
        let ordinal = self.user_prompt_ordinal();
        let mut message = Message::user(text.clone());
        message.ui = ui.clone();
        self.messages.push(message);
        self.persist_user_attribution(ordinal, &ui);
        self.pending_prompts.push(text);
        self.last_user_ui = ui;
    }

    pub fn insert_user_message_with_content_and_ui_metadata(
        &mut self,
        content: Vec<MessageContent>,
        ui: Option<MessageUiMetadata>,
    ) {
        // Text blocks join the prompt text; image blocks ride the next
        // prompt as kernel `ContentBlock::Image` (TS `prompt(text, { images })`
        // parity).
        let ordinal = self.user_prompt_ordinal();
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
        self.persist_user_attribution(ordinal, &ui);
        if !text.trim().is_empty() {
            self.pending_prompts.push(text);
        }
        if !images.is_empty() {
            self.pending_images.extend(images);
        }
        self.last_user_ui = ui;
    }

    pub fn enqueue_steer(
        &mut self,
        content: Vec<MessageContent>,
        ui: Option<MessageUiMetadata>,
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
        // The canonical message joins history at the next refresh (pi owns the
        // transcript); the workspace renders the optimistic bubble until
        // `SteerInjected` confirms.
        id
    }

    pub fn run_turn(&mut self) {
        if self.running || (self.pending_prompts.is_empty() && self.pending_images.is_empty()) {
            return;
        }
        self.ensure_engine(self.project.clone());
        let prompt = std::mem::take(&mut self.pending_prompts).join("\n\n");
        let images = std::mem::take(&mut self.pending_images);
        self.running = true;
        self.pending_events.push(ThreadEvent::TurnStarted);
        self.engine
            .as_ref()
            .expect("ensure_engine materialized the engine")
            .run(prompt, images);
    }

    /// Explicit user cancel (Go-style cancel context): aborts the active
    /// run, stops every background task this thread owns with TaskStop
    /// semantics, and aborts every spawned TeamMember's active turn — the
    /// member's own `cancel` recurses into its derivatives. Natural turn
    /// settle never reaches this path, so background work survives turns.
    pub fn cancel(&mut self) {
        if let Some(engine) = &self.engine {
            engine.abort();
            engine.abort_spawned_members();
        }
        // The runtime is initialized before any UI/actor cancel can fire;
        // tests that drive a facade without it hold no real tasks to stop.
        if let Some(handle) = crate::runtime::try_handle() {
            let thread_id = self.id.0.clone();
            handle.spawn(async move {
                crate::background_task::stop_all_for_thread(&thread_id).await;
            });
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Whether the facade holds queued prompts/images that the next
    /// `run_turn` would drain (the actor uses it to decide whether a
    /// post-settlement follow-up turn is needed).
    pub fn has_pending_prompts(&self) -> bool {
        !self.pending_prompts.is_empty() || !self.pending_images.is_empty()
    }

    /// Test-support: replace the lazily spawned backend with a scripted
    /// engine and wire its notice channel, so downstream host tests can
    /// observe run/steer traffic and drive settlement without a live
    /// provider.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_engine_for_test(
        &mut self,
        engine: Arc<dyn ThreadEngine>,
        events: tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
    ) {
        self.engine = Some(engine);
        self.pending_engine_events = Some(events);
    }

    /// Test-only: force the running flag so team routing tests can simulate
    /// a busy thread without a live engine turn. test-support exposure lets
    /// host integration tests park a thread by attaching another one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_running_for_test(&mut self, running: bool) {
        self.running = running;
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

    pub fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
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

    /// The current turn's total, keyed off the triggering user message; the
    /// conversation footer stamps it onto the just-finished reply at `Stop`.
    pub fn last_request_token_usage(&self) -> Option<TokenUsage> {
        let id = self
            .messages
            .iter()
            .rev()
            .find(|m| {
                matches!(m.role, crate::language_model::Role::User)
                    && m.provenance == crate::message::MessageProvenance::User
            })?
            .id
            .clone();
        self.request_usage.get(&id).copied()
    }

    pub fn per_model_last_request_usage(&self) -> HashMap<String, TokenUsage> {
        self.engine
            .as_ref()
            .map(|e| e.per_model_last_request_usage())
            .unwrap_or_default()
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

    /// The engine's display sequence: messages interleaved with the UI
    /// annotation cards at their persisted position. Empty until the first
    /// mirror refresh (landing threads have no engine).
    pub fn display_history(&self) -> &[HistoryEntry] {
        &self.display
    }

    /// Persist a UI annotation card as a session `custom` entry (fire and
    /// forget; the actor queue orders it against prompts). The facade mirror
    /// takes the card immediately so a switch-back before the next engine
    /// notice already renders it. No engine (landing thread): live-only.
    pub fn append_ui_note(&mut self, record: UiNoteRecord) {
        self.display.push(HistoryEntry::Note(record.clone()));
        if let Some(engine) = &self.engine {
            engine.append_ui_note(record);
        }
    }

    /// The thread's persisted Goal — one shared snapshot with the engine's
    /// goal tools (model-side writes land here too).
    pub fn goal(&self) -> Option<ThreadGoal> {
        self.goal_bridge.as_ref().and_then(|bridge| bridge.snapshot())
    }

    /// Elapsed seconds shown on the goal chip. There is no persisted time
    /// budget: the display is creation-anchored wall time while live, and the
    /// created→updated span once the goal reaches a terminal status.
    pub fn goal_elapsed_seconds(&self) -> Option<u64> {
        let goal = self.goal()?;
        if goal.status.is_terminal() {
            Some((goal.updated_at - goal.created_at).max(0) as u64)
        } else {
            let now = chrono::Utc::now().timestamp();
            Some((now - goal.created_at).max(0) as u64)
        }
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
        max_rounds: Option<u64>,
        actor: crate::goal::GoalActor,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        bridge.create_goal(objective, token_budget, max_rounds, actor)?;
        self.ensure_engine(self.project.clone());
        if let Some(engine) = &self.engine {
            engine.goal_started();
        }
        self.pending_events.push(ThreadEvent::GoalChanged {
            goal: self.goal(),
        });
        Ok(())
    }

    /// Convenience for `/goal <objective>`: create with no budget and no
    /// round cap as the user.
    pub fn set_goal(&mut self, objective: String) -> anyhow::Result<()> {
        self.create_goal(objective, None, None, crate::goal::GoalActor::User)
    }

    /// Edit objective/budget/rounds in place (keeps id and status).
    pub fn edit_goal(
        &mut self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: crate::goal::GoalActor,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        let goal = bridge.edit_goal(objective, token_budget, max_rounds, actor)?;
        self.pending_events.push(ThreadEvent::GoalChanged { goal: Some(goal) });
        Ok(())
    }

    /// Replace an unfinished Goal with a fresh one (explicit `/goal
    /// replace` confirmation path).
    pub fn replace_goal(
        &mut self,
        objective: String,
        token_budget: Option<u64>,
        max_rounds: Option<u64>,
        actor: crate::goal::GoalActor,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        bridge.replace_goal(objective, token_budget, max_rounds, actor)?;
        self.ensure_engine(self.project.clone());
        if let Some(engine) = &self.engine {
            engine.goal_started();
        }
        self.pending_events.push(ThreadEvent::GoalChanged {
            goal: self.goal(),
        });
        Ok(())
    }

    /// Pause/resume/blocked transitions with the domain guards. A transition
    /// to Active (resume) wakes the goal gate so automatic rounds resume.
    pub fn set_goal_status(
        &mut self,
        status: crate::goal::GoalStatus,
        reason: Option<crate::goal::GoalBlockReason>,
        actor: crate::goal::GoalActor,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        let was_active = self.goal().is_some_and(|goal| goal.status == crate::goal::GoalStatus::Active);
        let goal = bridge.set_goal_status(status, reason, actor)?;
        if status == crate::goal::GoalStatus::Active && !was_active
            && let Some(engine) = &self.engine
        {
            engine.goal_gate();
        }
        self.pending_events.push(ThreadEvent::GoalChanged { goal: Some(goal) });
        Ok(())
    }

    /// Clear the current Goal (tombstone event; history stays in the stream).
    pub fn clear_goal(
        &mut self,
        actor: crate::goal::GoalActor,
    ) -> anyhow::Result<()> {
        let bridge = self.goal_bridge_or_bail()?;
        bridge.clear_goal(actor)?;
        self.pending_events.push(ThreadEvent::GoalChanged { goal: None });
        Ok(())
    }


    pub fn depth(&self) -> u32 {
        0
    }

    pub fn agent_label(&self) -> &str {
        &self.label
    }

    /// The routing identity of the agent this thread runs (`Lead` for the
    /// main thread, the member name for team workers) as an attribution
    /// value for harness-seeded user turns.
    pub fn self_author(&self) -> crate::message::MessageAuthor {
        crate::team::author_for(&self.label)
    }

    /// Deliver peer messages from teammates: render each through the peer
    /// wrapper template, append it as a user message to the conversation,
    /// and emit [`ThreadEvent::PeerMessage`]. A delivery landing mid-turn
    /// stays in the history until the running turn settles; the model sees
    /// it on the next prompt assembly.
    pub fn deliver_peer_messages(
        &mut self,
        msgs: Vec<crate::team::PeerMessage>,
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
            let ui = MessageUiMetadata {
                author: Some(crate::team::author_for(&msg.from)),
                peer: true,
                display_text: Some(msg.content.clone()),
                ..Default::default()
            };
            self.insert_user_message_with_ui_metadata(rendered, Some(ui));
            self.pending_events.push(ThreadEvent::PeerMessage {
                from: msg.from.clone(),
                content: msg.content.clone(),
            });
        }
        self.run_turn();
    }

    /// Construct a team worker thread: a fresh pi session inheriting this
    /// (leader) thread's cwd / model / permission mode / reasoning effort,
    /// labeled with the member name. Engine spawned eagerly (members always
    /// run). Members carry no goal bridge: the goal contract belongs to the
    /// leader's user-facing conversation. The member session header records
    /// this leader's session id (`team.parent`), persisting the affiliation
    /// with the jsonl file so it survives restarts and team disband.
    pub fn new_team_member(&self, name: String) -> ThreadHandle {
        let id = ThreadId(uuid::Uuid::new_v4().to_string());
        let cwd = self.cwd.clone();
        let model = self.model.clone();
        let permission_mode = self.permission_mode;
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
            Some(self.id.0.clone()),
        );
        if permission_mode != PermissionMode::default() {
            engine.set_permission_mode(permission_mode);
        }
        if reasoning_effort != ReasoningEffort::default() {
            engine.set_thinking_level(Some(reasoning_effort.wire_value().to_string()));
        }
        let handle = ThreadHandle::new(Self {
            id,
            cwd,
            project: None,
            model,
            permission_mode,
            messages: Vec::new(),
            reasoning_effort,
            pinned: false,
            archived: false,
            running: false,
            restored: false,
            display: Vec::new(),
            request_usage: HashMap::new(),
            pending_prompts: Vec::new(),
            pending_images: Vec::new(),
            pending_steers: VecDeque::new(),
            last_user_ui: None,
            engine: Some(engine),
            history_phase: HistoryPhase::Ready,
            permission_mode_explicitly_set: true,
            reasoning_effort_explicitly_set: true,
            browser_suites: Vec::new(),
            // Members never mount browser suites; the explicit flag keeps
            // even an empty-mirror seed from the Ready projection out.
            browser_suites_explicitly_set: true,
            plan_mode: false,
            persisted_plan: None,
            goal_bridge: None,
            label: name,
            cwd_path: None,
            pending_events: Vec::new(),
            pending_engine_events: None,
        });
        drain_engine_notices(handle.clone(), events);
        handle
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
    ) {
        self.ensure_engine(self.project.clone());
        if let Some(engine) = &self.engine {
            engine.start_plan_execution(plan_file);
        }
        self.insert_user_message_with_ui_metadata(seed_text, ui);
        self.run_turn();
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

    /// The session's effective working directory (mirrored from the
    /// engine's `CwdChanged` events); `None` until the engine reports one.
    pub fn cwd_path(&self) -> Option<&str> {
        self.cwd_path.as_deref()
    }

    /// Persist whether a plan review card is pending, so a restarted
    /// session re-surfaces the card (the card itself is UI-only state).
    pub fn set_plan_review_pending(&mut self, pending: bool) {
        if let Some(engine) = &self.engine {
            engine.set_plan_review_pending(pending);
        }
    }

    /// Toggle plan mode: the engine persists the flag in the session
    /// sidecar, wires the read-only gate, and injects the plan-mode
    /// instructions (rendered for the configured agent language) every turn.
    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.plan_mode = enabled;
        if let Some(engine) = &self.engine {
            engine.set_plan_mode(enabled);
        }
    }

    /// Toggle an opt-in browser tool suite (ChromeUse / WebExplore) on or off.
    pub fn set_browser_suite(
        &mut self,
        suite: crate::pi_engine::BrowserSuite,
        enable: bool,
    ) {
        self.browser_suites_explicitly_set = true;
        let changed = if enable {
            if self.browser_suites.contains(&suite) {
                false
            } else {
                self.browser_suites.push(suite);
                true
            }
        } else {
            let before = self.browser_suites.len();
            self.browser_suites.retain(|s| *s != suite);
            self.browser_suites.len() != before
        };
        // A landing thread parks the toggle in the mirror; `ensure_engine`
        // replays it when the engine materializes.
        if let Some(engine) = &self.engine {
            engine.set_browser_suite(suite, enable);
        }
        if changed {
            self.pending_events.push(ThreadEvent::BrowserSuitesChanged {
                suites: self.browser_suites.clone(),
            });
        }
    }

    /// The opt-in browser tool suites active on this thread; the workspace
    /// derives the composer chips from this mirror.
    pub fn browser_suites(&self) -> &[crate::pi_engine::BrowserSuite] {
        &self.browser_suites
    }

    /// Execute an approved plan on this thread: optionally compact the
    /// planning context first (distilled toward the plan file), then run
    /// the execution seed turn.
    pub fn approve_plan(
        &mut self,
        compact: bool,
        compact_instructions: Option<String>,
        seed_text: String,
        ui: Option<MessageUiMetadata>,
    ) {
        self.plan_mode = false;
        // The engine injects the seed into its own transcript; record the
        // attribution now so the mirrored history keeps it after refresh.
        self.persist_user_attribution(self.user_prompt_ordinal(), &ui);
        self.last_user_ui = ui;
        if let Some(engine) = &self.engine {
            engine.approve_plan(compact, compact_instructions, seed_text);
        }
    }

    /// Move the session's working directory at any interaction state —
    /// the host-driven `SetCwd` path. Unlike [`Thread::set_project`] (an
    /// initial-only project binding, guarded by `has_interacted`), this
    /// follows the per-call cwd machinery: the sticky cwd advances and the
    /// move is durable as a `cwd_change` entry, never touching the header
    /// cwd or the project binding.
    pub fn set_cwd(&self, path: PathBuf) {
        if let Some(engine) = &self.engine {
            engine.set_cwd(path);
        }
    }

    pub fn set_project(&mut self, dir: PathBuf) {
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
            self.ensure_engine(Some(dir));
        }
    }

    /// Manual compaction (`/compact`): no-op while a turn is in flight (the
    /// kernel compacts an idle transcript only); the recap card lands via the
    /// engine's harness-event adaptation.
    pub fn compact(&mut self, custom_instructions: Option<String>) {
        if self.running {
            return;
        }
        if let Some(engine) = &self.engine {
            engine.compact(custom_instructions);
        }
    }

    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        if self.permission_mode == mode {
            return;
        }
        self.permission_mode = mode;
        // An explicit user choice must survive engine materialization: the
        // session sidecar's default would otherwise overwrite it at `Ready`.
        self.permission_mode_explicitly_set = true;
        if let Some(engine) = &self.engine {
            // The engine applies the mode to its gate and persists it in the
            // session sidecar; the chip reflects the change immediately.
            engine.set_permission_mode(mode);
        }
        self.pending_events.push(ThreadEvent::PermissionModeChanged { mode });
    }

    /// Deliver the user's answer for a pending interaction card
    /// (`AskUserQuestion`). Unknown ids are ignored.
    pub fn respond_authorization(
        &mut self,
        id: &str,
        response: crate::permission::ToolAuthorizationResponse,
    ) {
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

    pub fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
    }

    pub fn set_model(&mut self, model: PiModel) {
        let from = self.model.as_ref().map(|m| m.id.clone());
        let to = model.id.clone();
        self.model = Some(model.clone());
        if let Some(engine) = &self.engine {
            engine.set_model(model);
        }
        self.pending_events.push(ThreadEvent::ModelChanged { from, to });
    }

    pub fn set_reasoning_effort(&mut self, effort: ReasoningEffort) {
        if self.reasoning_effort == effort {
            return;
        }
        self.reasoning_effort = effort;
        // An explicit user choice must survive engine materialization: the
        // session sidecar's default would otherwise overwrite it at `Ready`.
        self.reasoning_effort_explicitly_set = true;
        if let Some(engine) = &self.engine {
            engine.set_thinking_level(Some(effort.wire_value().to_string()));
        }
        self.pending_events.push(ThreadEvent::ReasoningEffortChanged { effort });
    }

    pub fn set_archived(&mut self, archived: bool) {
        self.archived = archived;
    }
}

/// Drain a spawned engine's notice channel on the runtime, dispatching each
/// notice through the thread's `handle_notice` (re-homed onto the handle in a
/// later slice). Shared by `open` (engine present at construction) and
/// `ensure_engine` (landing materialization).
fn drain_engine_notices(
    handle: ThreadHandle,
    mut events: tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
) {
    crate::runtime::handle().spawn(async move {
        while let Some(notice) = events.recv().await {
            handle.handle_notice(notice);
        }
    });
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
    ) -> bool {
        let Some(cmd) = crate::command::global().get(name).cloned() else {
            return false;
        };
        self.insert_slash_turn(cmd.render(args), ui);
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
        self.insert_slash_turn(rendered, ui);
        true
    }

    /// Run a built-in slash command (`/mode`, `/plan`, `/compact`,
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
    ) -> bool {
        match crate::slash_builtins::canonical_builtin(name).map(|meta| meta.name) {
            Some("mode") => {
                let trimmed = args.trim();
                if trimmed.is_empty() {
                    let next = match self.permission_mode() {
                        PermissionMode::ReadOnly => PermissionMode::WorkspaceWrite,
                        PermissionMode::WorkspaceWrite => PermissionMode::DangerFullAccess,
                        PermissionMode::DangerFullAccess => PermissionMode::ReadOnly,
                    };
                    self.set_permission_mode(next);
                } else {
                    let (name, rest) = match trimmed.split_once(char::is_whitespace) {
                        Some((head, tail)) => (head, tail.trim()),
                        None => (trimmed, ""),
                    };
                    let parsed: Result<PermissionMode, _> =
                        serde_json::from_value(serde_json::Value::String(name.to_string()));
                    match parsed {
                        Ok(mode) => {
                            self.set_permission_mode(mode);
                            if !rest.is_empty() {
                                self.insert_slash_turn(rest.to_string(), ui);
                            }
                        }
                        Err(_) => {
                            self.pending_events.push(ThreadEvent::Error(anyhow::anyhow!(
                                "unknown permission mode `{name}` (expected read-only, \
                                 workspace-write, or danger-full-access)"
                            )));
                        }
                    }
                }
                true
            }
            Some("plan") => {
                if self.plan_mode() {
                    self.set_plan_mode(false);
                } else {
                    self.set_plan_mode(true);
                    if !args.trim().is_empty() {
                        self.insert_slash_turn(args.to_string(), ui);
                    }
                }
                true
            }
            Some("compact") => {
                let trimmed = args.trim();
                let instructions = (!trimmed.is_empty()).then(|| trimmed.to_string());
                self.compact(instructions);
                true
            }
            Some("goal") => {
                self.run_goal_slash(args, ui);
                true
            }
            _ => false,
        }
    }

    /// `/goal` subcommand dispatch. Thread-state transitions mirror the gpui
    /// host's `GoalCommand`; the popover-only forms (bare `/goal`, bare
    /// `edit`/`replace`) are no-ops here — the webview surfaces goal state
    /// through its info card instead.
    fn run_goal_slash(&mut self, args: &str, ui: Option<MessageUiMetadata>) {
        let trimmed = args.trim();
        if let Some(objective) = trimmed.strip_prefix("replace ").map(str::trim) {
            if let Err(error) = self.replace_goal(objective.to_string(), None, None, crate::goal::GoalActor::User)
            {
                self.pending_events.push(ThreadEvent::Error(error));
            }
            return;
        }
        if let Some(objective) = trimmed.strip_prefix("edit ").map(str::trim) {
            let current = self.goal();
            let budget = current.as_ref().and_then(|goal| goal.token_budget);
            let max_rounds = current.as_ref().and_then(|goal| goal.max_rounds);
            if let Err(error) = self.edit_goal(objective.to_string(), budget, max_rounds, crate::goal::GoalActor::User)
            {
                self.pending_events.push(ThreadEvent::Error(error));
            }
            return;
        }
        if let Some(value) = trimmed.strip_prefix("budget ").map(str::trim) {
            let Some(goal) = self.goal() else {
                self.pending_events.push(ThreadEvent::Error(anyhow::anyhow!("thread has no Goal")));
                return;
            };
            let budget = if matches!(value, "none" | "unlimited") {
                None
            } else {
                match value.parse::<u64>() {
                    Ok(value) => Some(value),
                    Err(error) => {
                        self.pending_events.push(ThreadEvent::Error(error.into()));
                        return;
                    }
                }
            };
            if let Err(error) = self.edit_goal(goal.objective, budget, goal.max_rounds, crate::goal::GoalActor::User)
            {
                self.pending_events.push(ThreadEvent::Error(error));
            }
            return;
        }
        if let Some(value) = trimmed.strip_prefix("rounds ").map(str::trim) {
            let Some(goal) = self.goal() else {
                self.pending_events.push(ThreadEvent::Error(anyhow::anyhow!("thread has no Goal")));
                return;
            };
            let max_rounds = if matches!(value, "none" | "unlimited") {
                None
            } else {
                match value.parse::<u64>() {
                    Ok(value) => Some(value),
                    Err(error) => {
                        self.pending_events.push(ThreadEvent::Error(error.into()));
                        return;
                    }
                }
            };
            if let Err(error) = self.edit_goal(goal.objective, goal.token_budget, max_rounds, crate::goal::GoalActor::User)
            {
                self.pending_events.push(ThreadEvent::Error(error));
            }
            return;
        }
        match trimmed.to_lowercase().as_str() {
            "" | "edit" | "replace" | "rounds" => {}
            "clear" => {
                if let Err(error) = self.clear_goal(crate::goal::GoalActor::User) {
                    self.pending_events.push(ThreadEvent::Error(error));
                }
            }
            "pause" | "stop" => {
                if let Err(error) = self.set_goal_status(
                    crate::goal::GoalStatus::Paused,
                    Some(crate::goal::GoalBlockReason {
                        code: "user-paused".into(),
                        message: "paused by user".into(),
                    }),
                    crate::goal::GoalActor::User,
                ) {
                    self.pending_events.push(ThreadEvent::Error(error));
                }
            }
            "resume" => {
                if let Err(error) = self.set_goal_status(
                    crate::goal::GoalStatus::Active,
                    None,
                    crate::goal::GoalActor::User,
                ) {
                    self.pending_events.push(ThreadEvent::Error(error));
                }
            }
            _ => match self.set_goal(trimmed.to_string()) {
                Ok(()) => self.insert_slash_turn(trimmed.to_string(), ui),
                Err(error) => self.pending_events.push(ThreadEvent::Error(error)),
            },
        }
    }

    /// Insert a user turn and run it, persisting the compact `/name args`
    /// display form — the shared tail of registry and built-in slash turns.
    fn insert_slash_turn(
        &mut self,
        text: String,
        ui: Option<MessageUiMetadata>,
    ) {
        let ordinal = self.user_prompt_ordinal();
        let display = ui.as_ref().and_then(|ui| ui.display_text.clone());
        self.insert_user_message_with_ui_metadata(text, ui);
        self.persist_registry_display(ordinal, display);
        self.run_turn();
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

    /// Persist the attribution of an injected user turn in the session
    /// sidecar so a reload restores the send-time header. Same ordinal
    /// convention as `persist_registry_display`; a compaction clears both.
    fn persist_user_attribution(&self, ordinal: usize, ui: &Option<MessageUiMetadata>) {
        let Some(ui) = ui else {
            return;
        };
        let Some(author) = &ui.author else {
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
        let record = pi_extensions::session_meta::UserAttributionMeta {
            author: author.routing().to_string(),
            peer: ui.peer,
            display_text: ui.display_text.clone(),
        };
        persist_user_attribution_spawn(sessions_dir, session_path, ordinal, record);
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
    pub fn open_session(&mut self, path: PathBuf) {
        if let Some(engine) = &self.engine {
            engine.open_session(path);
        }
        self.running = false;
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
        if let Err(err) =
            pi_extensions::session_meta::update(&sessions_dir, &session_path, |meta| {
                meta.registry_displays.insert(ordinal, display);
            })
            .await
        {
            tracing::warn!(error = %err, "failed to persist registry display text");
        }
    })
}

fn persist_user_attribution_spawn(
    sessions_dir: PathBuf,
    session_path: PathBuf,
    ordinal: usize,
    record: pi_extensions::session_meta::UserAttributionMeta,
) -> tokio::task::JoinHandle<()> {
    crate::runtime::handle().spawn(async move {
        if let Err(err) =
            pi_extensions::session_meta::update(&sessions_dir, &session_path, |meta| {
                meta.user_attributions.insert(ordinal, record);
            })
            .await
        {
            tracing::warn!(error = %err, "failed to persist user message attribution");
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
        abort_calls: AtomicUsize,
        permission_mode: Mutex<Option<PermissionMode>>,
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
                abort_calls: AtomicUsize::new(0),
                permission_mode: Mutex::new(None),
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

        fn history(&self) -> Vec<HistoryEntry> {
            self.history.clone().into_iter().map(HistoryEntry::Message).collect()
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

        fn abort(&self) {
            self.abort_calls.fetch_add(1, Ordering::Relaxed);
        }

        fn set_model(&self, _model: PiModel) {}

        fn set_thinking_level(&self, level: Option<String>) {
            *self.thinking_level.lock().unwrap() = level;
        }

        fn open_session(&self, _path: PathBuf) {}

        fn new_session(&self, _cwd: PathBuf, _project: Option<PathBuf>) {}

        fn set_cwd(&self, _path: PathBuf) {}

        fn active_session_path(&self) -> Option<PathBuf> {
            None
        }

        fn session_list(&self) -> Vec<crate::db::ThreadSummary> {
            Vec::new()
        }

        fn shutdown(&self) {
            self.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn set_permission_mode(&self, mode: PermissionMode) {
            *self.permission_mode.lock().unwrap() = Some(mode);
        }
    }

    /// Construct a `Thread` facade directly (no actor) with the given
    /// history-loading phase and engine, so the notice-contract tests below
    /// exercise `handle_notice` in isolation.
    pub(crate) fn thread_with_engine(
        phase: HistoryPhase,
        engine: Arc<dyn ThreadEngine>,
    ) -> ThreadHandle {
        ThreadHandle::new(Thread {
            id: ThreadId("test-thread".to_string()),
            cwd: PathBuf::from("/tmp"),
            project: None,
            model: None,
            permission_mode: PermissionMode::default(),
            messages: Vec::new(),
            reasoning_effort: ReasoningEffort::default(),
            pinned: false,
            archived: false,
            running: false,
            restored: false,
            display: Vec::new(),
            request_usage: HashMap::new(),
            pending_prompts: Vec::new(),
            pending_images: Vec::new(),
            pending_steers: VecDeque::new(),
            last_user_ui: None,
            engine: Some(engine),
            history_phase: phase,
            permission_mode_explicitly_set: false,
            reasoning_effort_explicitly_set: false,
            browser_suites: Vec::new(),
            browser_suites_explicitly_set: false,
            plan_mode: false,
            persisted_plan: None,
            label: "lead".into(),
            goal_bridge: None,
            cwd_path: None,
            pending_events: Vec::new(),
            pending_engine_events: None,
        })
    }

    #[test]
    fn permission_mode_maps_i64_roundtrip() {
        assert_eq!(PermissionMode::from_i64(0), PermissionMode::ReadOnly);
        assert_eq!(PermissionMode::from_i64(1), PermissionMode::WorkspaceWrite);
        assert_eq!(PermissionMode::from_i64(2), PermissionMode::DangerFullAccess);
        assert_eq!(PermissionMode::ReadOnly.as_i64(), 0);
        assert_eq!(PermissionMode::WorkspaceWrite.as_i64(), 1);
        assert_eq!(PermissionMode::DangerFullAccess.as_i64(), 2);
        // Unknown persisted values land on the bounded default.
        assert_eq!(PermissionMode::from_i64(-1), PermissionMode::default());
        assert_eq!(PermissionMode::from_i64(3), PermissionMode::default());
        // Wire names are the kebab sidecar values.
        assert_eq!(
            serde_json::to_value(PermissionMode::DangerFullAccess).unwrap(),
            serde_json::json!("danger-full-access")
        );
        assert_eq!(
            serde_json::from_value::<PermissionMode>(serde_json::json!("read-only")).unwrap(),
            PermissionMode::ReadOnly
        );
    }

    /// Persistence must dispatch through Manox's process-global runtime even
    /// when no Tokio reactor is entered on the calling thread. Kept a plain
    /// `#[test]` (not `#[tokio::test]`) on purpose: entering a reactor here
    /// would defeat the very condition under test.
    #[test]
    fn registry_display_persistence_dispatches_off_runtime() {
        crate::runtime::init();

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
    #[tokio::test]
    async fn live_history_refreshes_mirror_and_actor_turn_started_sets_running() {
        let engine = Arc::new(FakeEngine {
            history: vec![Message::assistant(vec![MessageContent::Text(
                "partial answer".into(),
            )])],
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine);
        // Mid-run mirror refresh: the live partial lands in `messages`.
        thread.handle_notice(BackendNotice::LiveHistory);
        thread.read(|t| {
            assert_eq!(t.messages.len(), 1);
            assert!(matches!(
                &t.messages[0].content[0],
                MessageContent::Text(text) if text == "partial answer"
            ));
            assert!(!t.is_running());
        });

        // Actor-initiated run start mirrors onto the facade running flag.
        thread.handle_notice(BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)));
        thread.read(|t| assert!(t.is_running()));

        // Settlement releases the slot (and refreshes the mirror).
        thread.handle_notice(BackendNotice::Settled {
            cancelled: false,
            failed: false,
            steered: Vec::new(),
            stranded: Vec::new(),
        });
        thread.read(|t| assert!(!t.is_running()));
    }

    /// The headless slash router drives the same thread state the gpui host
    /// toggles: plan mode, permission mode, compact, and goal lifecycle.
    #[tokio::test]
    async fn run_slash_builtin_plan_mode_compact_and_unowned() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone());
        // The prompt form enters plan mode and runs the turn with the
        // compact display form.
        let ui = MessageUiMetadata {
            display_text: Some("/plan fix it".into()),
            ..Default::default()
        };
        assert!(thread.with_mut(|t| t.run_slash_builtin("plan", "fix it", Some(ui.clone()))));
        thread.read(|t| assert!(t.plan_mode(), "/plan <prompt> enters plan mode"));
        let runs = engine.runs.lock().unwrap();
        assert_eq!(runs.len(), 1, "prompt form runs a turn");
        assert_eq!(runs[0].0, "fix it");
        drop(runs);
        thread.read(|t| {
            let last = t.messages().last().expect("turn message inserted");
            assert_eq!(
                last.ui.as_ref().and_then(|ui| ui.display_text.as_deref()),
                Some("/plan fix it")
            );
        });

        // A second invocation toggles plan mode back off, no new turn.
        assert!(thread.with_mut(|t| t.run_slash_builtin("plan", "", None)));
        thread.read(|t| assert!(!t.plan_mode(), "/plan bare exits plan mode"));
        assert_eq!(engine.runs.lock().unwrap().len(), 1);

        assert!(thread.with_mut(|t| t.run_slash_builtin("mode", "", None)));
        thread.read(|t| assert_eq!(t.permission_mode(), PermissionMode::DangerFullAccess));
        // Named form sets the mode directly.
        assert!(thread.with_mut(|t| t.run_slash_builtin("mode", "read-only", None)));
        thread.read(|t| assert_eq!(t.permission_mode(), PermissionMode::ReadOnly));
        assert!(thread.with_mut(|t| t.run_slash_builtin("compact", "focus", None)));
        assert!(thread.with_mut(|t| t.run_slash_builtin("goal", "clear", None)));

        // Session-level commands and unknowns are not owned here.
        assert!(!thread.with_mut(|t| t.run_slash_builtin("exit", "", None)));
        assert!(!thread.with_mut(|t| t.run_slash_builtin("quit", "", None)));
        assert!(!thread.with_mut(|t| t.run_slash_builtin("new", "", None)));
        assert!(!thread.with_mut(|t| t.run_slash_builtin("nope", "", None)));
    }

    #[tokio::test]
    async fn live_history_reattaches_ui_metadata_to_prompt_not_tool_result() {
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
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine);
        thread.with_mut(|t| {
            t.insert_user_message_with_ui_metadata(
                "expanded registry prompt".to_string(),
                Some(MessageUiMetadata {
                    display_text: Some("/gitwork:deliver fast".to_string()),
                    ..Default::default()
                }),
            );
        });
        thread.handle_notice(BackendNotice::LiveHistory);

        thread.read(|t| {
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
    #[tokio::test]
    async fn run_turn_drains_pending_text_and_images() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone());
        thread.with_mut(|t| {
            t.insert_user_message_with_content_and_ui_metadata(
                vec![
                    MessageContent::Text("look at these".to_string()),
                    png_image("aW1hZ2Ux"),
                    png_image("aW1hZ2Uy"),
                ],
                None,
            );
            t.run_turn();
        });
        let runs = engine.runs.lock().unwrap();
        assert_eq!(runs.len(), 1, "exactly one turn ran");
        let (prompt, images) = &runs[0];
        assert_eq!(prompt, "look at these");
        assert_eq!(images.len(), 2, "both images ride the turn");
        drop(runs);
        thread.read(|t| {
            assert!(t.pending_prompts.is_empty(), "text queue drained");
            assert!(t.pending_images.is_empty(), "image queue drained");
        });
    }

    #[tokio::test]
    async fn sailor_completed_notice_injects_peer_message_and_fires_turn() {
        let engine = Arc::new(FakeEngine::new());
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone());
        thread.handle_notice(BackendNotice::SteerDelivered {
            from: pi_extensions::steer_bus::AgentId::Subagent("sailor-1".into()),
            reason: pi_extensions::steer_bus::SteerReason::Complete,
            payload: pi_extensions::steer_bus::SteerPayload {
                text: "PR #601 LGTM".into(),
            },
        });
        let runs = engine.runs.lock().unwrap();
        assert_eq!(runs.len(), 1, "the Sailor completion fired one turn");
        assert!(runs[0].0.contains("PR #601 LGTM"), "final text rode the prompt: {:?}", runs[0].0);
        drop(runs);
        let has_user = thread.read(|t| {
            t.messages.iter().any(|m| matches!(m.role, crate::language_model::Role::User))
        });
        assert!(has_user, "a user message was injected for the completion");
    }

    /// An image-only insert (no text) still starts a turn — the guard keys on
    /// BOTH queues being empty, so the engine receives an empty prompt plus
    /// the image (kernel pushes the empty text block, TS parity).
    #[tokio::test]
    async fn run_turn_image_only_insert_still_starts_turn() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone());
        thread.with_mut(|t| {
            t.insert_user_message_with_content_and_ui_metadata(
                vec![png_image("aW1hZ2Ux")],
                None,
            );
            t.run_turn();
        });
        let runs = engine.runs.lock().unwrap();
        assert_eq!(runs.len(), 1, "image-only turn ran");
        let (prompt, images) = &runs[0];
        assert_eq!(prompt, "", "no text was queued");
        assert_eq!(images.len(), 1);
    }

    /// With neither text nor images pending, `run_turn` is a no-op.
    #[tokio::test]
    async fn run_turn_noop_when_nothing_pending() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone());
        thread.with_mut(|t| t.run_turn());
        assert!(engine.runs.lock().unwrap().is_empty(), "no turn ran");
    }

    /// The race-fix contract: `Ready` — regardless of preview batches that
    /// may have streamed before it — replaces the mirror with the engine's
    /// authoritative history and clears `Loading`, so the workspace leaves
    /// the spinner and re-enables input.
    #[tokio::test]
    async fn ready_replaces_preview_with_authoritative_history_and_clears_loading() {
        let engine = Arc::new(FakeEngine {
            history: vec![Message::user("authoritative".to_string())],
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Loading, engine);
        // Simulate a preview batch that landed before the authoritative sync.
        thread.with_mut(|t| {
            t.messages = vec![Message::user("preview-only".to_string())];
        });
        thread.handle_notice(BackendNotice::Ready(Box::new(ReadyInfo {
            restored: true,
            model: None,
            permission_mode: PermissionMode::default(),
            reasoning_effort: ReasoningEffort::default(),
            browser_suites: Vec::new(),
            plan_mode: false,
            plan_file: None,
            plan_review_pending: false,
            plan_snapshot: None,
        })));
        let (phase, texts) = thread.read(|t| {
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
    #[tokio::test]
    async fn fatal_before_ready_clears_loading_and_preview() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Loading, engine);
        thread.with_mut(|t| {
            t.messages = vec![Message::user("stale-preview".to_string())];
        });
        thread.handle_notice(BackendNotice::Fatal(anyhow::anyhow!("no model configured")));
        let (phase, count) = thread.read(|t| (t.history_phase(), t.messages().len()));
        assert_eq!(phase, HistoryPhase::Ready, "input gate opens");
        assert_eq!(count, 0, "stale preview is dropped");
    }

    /// I3: an explicit user permission-mode choice on a landing thread
    /// survives the sidecar default arriving at `Ready`
    /// (`permission_mode_explicitly_set`).
    #[tokio::test]
    async fn explicit_permission_mode_survives_ready_sidecar_default() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine);
        thread.with_mut(|t| t.set_permission_mode(PermissionMode::ReadOnly));
        // The fresh session's sidecar reports the default at Ready; the
        // user's ReadOnly choice must not be overwritten.
        thread.handle_notice(BackendNotice::Ready(Box::new(ReadyInfo {
            restored: false,
            model: None,
            permission_mode: PermissionMode::default(),
            reasoning_effort: ReasoningEffort::default(),
            browser_suites: Vec::new(),
            plan_mode: false,
            plan_file: None,
            plan_review_pending: false,
            plan_snapshot: None,
        })));
        assert_eq!(
            thread.read(|t| t.permission_mode()),
            PermissionMode::ReadOnly
        );
    }

    /// P1: `PlanUpdated` mirrors onto the facade and persists through the
    /// engine; an empty snapshot clears both (the model dropped its plan).
    #[tokio::test]
    async fn plan_updated_mirrors_and_persists() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let engine_ref = Arc::clone(&engine);
        let thread = thread_with_engine(HistoryPhase::Ready, engine);

        let snapshot = crate::plan::PlanSnapshot {
            explanation: None,
            steps: vec![crate::plan::PlanStep {
                step: "investigate".to_string(),
                status: crate::plan::PlanStepStatus::InProgress,
            }],
        };
        thread.handle_notice(BackendNotice::Event(Box::new(
            crate::thread::ThreadEvent::PlanUpdated {
                snapshot: snapshot.clone(),
            },
        )));
        thread.read(|t| {
            assert_eq!(t.persisted_plan(), Some(&snapshot));
        });
        let persists = engine_ref.plan_persists.lock().unwrap();
        assert_eq!(persists.len(), 1);
        let stored: crate::plan::PlanSnapshot =
            serde_json::from_value(persists[0].clone().unwrap()).unwrap();
        assert_eq!(stored, snapshot);
        drop(persists);

        // Empty snapshot = the model cleared its plan → mirror + sidecar clear.
        thread.handle_notice(BackendNotice::Event(Box::new(
            crate::thread::ThreadEvent::PlanUpdated {
                snapshot: crate::plan::PlanSnapshot {
                    explanation: None,
                    steps: Vec::new(),
                },
            },
        )));
        thread.read(|t| {
            assert_eq!(t.persisted_plan(), None);
        });
        let persists = engine_ref.plan_persists.lock().unwrap();
        assert_eq!(persists.len(), 2);
        assert!(persists[1].is_none());
    }

    /// P2: `Ready` restores the persisted plan snapshot (post-restart /
    /// thread-switch source for the rail's fallback).
    #[tokio::test]
    async fn ready_restores_persisted_plan_snapshot() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Loading, engine);
        let snapshot = crate::plan::PlanSnapshot {
            explanation: None,
            steps: vec![crate::plan::PlanStep {
                step: "implement".to_string(),
                status: crate::plan::PlanStepStatus::Completed,
            }],
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        thread.handle_notice(BackendNotice::Ready(Box::new(ReadyInfo {
            restored: true,
            model: None,
            permission_mode: PermissionMode::default(),
            reasoning_effort: ReasoningEffort::default(),
            browser_suites: Vec::new(),
            plan_mode: false,
            plan_file: None,
            plan_review_pending: false,
            plan_snapshot: Some(value),
        })));
        thread.read(|t| {
            assert_eq!(t.persisted_plan(), Some(&snapshot));
        });
    }

    /// I4: an explicit user reasoning-effort choice survives the sidecar
    /// default arriving at `Ready` (`reasoning_effort_explicitly_set`).
    #[tokio::test]
    async fn explicit_reasoning_effort_survives_ready_sidecar_default() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine);
        thread.with_mut(|t| t.set_reasoning_effort(ReasoningEffort::Max));
        // The fresh session's sidecar reports High at Ready; the user's Max
        // choice must not be overwritten.
        thread.handle_notice(BackendNotice::Ready(Box::new(ReadyInfo {
            restored: false,
            model: None,
            permission_mode: PermissionMode::default(),
            reasoning_effort: ReasoningEffort::default(),
            browser_suites: Vec::new(),
            plan_mode: false,
            plan_file: None,
            plan_review_pending: false,
            plan_snapshot: None,
        })));
        assert_eq!(
            thread.read(|t| t.reasoning_effort()),
            ReasoningEffort::Max
        );
    }

    /// I5: without an explicit choice, `Ready` restores the persisted
    /// effort from the sidecar.
    #[tokio::test]
    async fn ready_restores_reasoning_effort_when_not_explicitly_set() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Loading, engine);
        thread.handle_notice(BackendNotice::Ready(Box::new(ReadyInfo {
            restored: true,
            model: None,
            permission_mode: PermissionMode::default(),
            reasoning_effort: ReasoningEffort::Max,
            browser_suites: Vec::new(),
            plan_mode: false,
            plan_file: None,
            plan_review_pending: false,
            plan_snapshot: None,
        })));
        assert_eq!(
            thread.read(|t| t.reasoning_effort()),
            ReasoningEffort::Max
        );
    }

    /// P0 regression: a browser-suite toggle on an engine-less landing
    /// thread parks in the facade mirror (replayed at `ensure_engine`)
    /// instead of being dropped.
    #[tokio::test]
    async fn landing_browser_suite_toggle_parks_in_mirror() {
        crate::pi_providers::init_for_test();
        let thread = Thread::landing(PathBuf::from("/tmp"));
        thread.with_mut(|t| {
            t.set_browser_suite(crate::pi_engine::BrowserSuite::ChromeUse, true);
        });
        thread.read(|t| {
            assert_eq!(
                t.browser_suites().to_vec(),
                vec![crate::pi_engine::BrowserSuite::ChromeUse]
            );
        });
    }

    /// The `Ready` projection seeds the suite mirror so restored sessions
    /// surface their active suites (the composer chips derive from it).
    #[tokio::test]
    async fn ready_seeds_browser_suites_from_projection() {
        let thread = thread_with_engine(HistoryPhase::Loading, Arc::new(FakeEngine::new()));
        thread.handle_notice(BackendNotice::Ready(Box::new(ReadyInfo {
            restored: true,
            model: None,
            permission_mode: PermissionMode::default(),
            reasoning_effort: ReasoningEffort::default(),
            browser_suites: vec![crate::pi_engine::BrowserSuite::ChromeUse],
            plan_mode: false,
            plan_file: None,
            plan_review_pending: false,
            plan_snapshot: None,
        })));
        thread.read(|t| {
            assert_eq!(
                t.browser_suites().to_vec(),
                vec![crate::pi_engine::BrowserSuite::ChromeUse]
            );
        });
    }

    /// A toggle since construction outranks the `Ready` projection: the
    /// queued engine command has not settled when Ready lands, so the
    /// projection cannot know about it and must not clobber the mirror.
    #[tokio::test]
    async fn explicit_browser_suite_toggle_outranks_ready_projection() {
        let thread = thread_with_engine(HistoryPhase::Ready, Arc::new(FakeEngine::new()));
        thread.with_mut(|t| {
            t.set_browser_suite(crate::pi_engine::BrowserSuite::WebExplore, true);
        });
        thread.handle_notice(BackendNotice::Ready(Box::new(ReadyInfo {
            restored: true,
            model: None,
            permission_mode: PermissionMode::default(),
            reasoning_effort: ReasoningEffort::default(),
            browser_suites: vec![crate::pi_engine::BrowserSuite::ChromeUse],
            plan_mode: false,
            plan_file: None,
            plan_review_pending: false,
            plan_snapshot: None,
        })));
        thread.read(|t| {
            assert_eq!(
                t.browser_suites().to_vec(),
                vec![crate::pi_engine::BrowserSuite::WebExplore]
            );
        });
    }

    /// `Thread::cancel` aborts the engine; the actor relies on this when
    /// disposal must not wait for the in-flight turn.
    #[tokio::test]
    async fn cancel_aborts_engine() {
        let engine = Arc::new(FakeEngine {
            history: Vec::new(),
            shutdown_calls: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            permission_mode: Mutex::new(None),
            thinking_level: Mutex::new(None),
            runs: Mutex::new(Vec::new()),
            plan_persists: Mutex::new(Vec::new()),
        });
        let thread = thread_with_engine(HistoryPhase::Ready, engine.clone());
        thread.with_mut(|t| t.cancel());
        assert_eq!(engine.abort_calls.load(Ordering::SeqCst), 1);
    }
}


