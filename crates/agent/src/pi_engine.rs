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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

use crate::db::{HistoryEntry, PositionedNote, ThreadSummary, UI_NOTE_CUSTOM_TYPE, UiNoteRecord};
use crate::language_model::{MessageContent, ReasoningEffort, TokenUsage};
use crate::message::Message;
use crate::permission::{PendingAuthMeta, ToolAuthorizationResponse};
use crate::pi_approval::{ApprovalGate, ApprovalGatedTool, PiAskUserQuestionTool};
use crate::thread::{PermissionMode, ThreadEvent};
use crate::thread_engine::{BackendNotice, ReadyInfo, SpawnedEngine, ThreadEngine};

/// How often the engine refreshes its history mirror while a run is in
/// flight. Bounds the mid-run staleness a thread switch-back can observe;
/// each tick clones the transcript, so the interval balances freshness
/// against churn on large sessions.
const LIVE_HISTORY_TICK: std::time::Duration = std::time::Duration::from_millis(500);

/// Commands the gpui side sends to the pi actor.
pub(crate) enum SessionCmd {
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
    /// Switch the permission mode and persist it in the session sidecar.
    SetPermissionMode(PermissionMode),
    /// Toggle an opt-in browser tool suite (ChromeUse / WebExplore) on or
    /// off; the engine merges/removes the suite names atomically.
    SetBrowserSuite { suite: BrowserSuite, enable: bool },
    /// Manual compaction (`/compact`), optionally steering the summary.
    Compact { custom_instructions: Option<String> },
    /// Toggle plan mode (persisted sidecar + hooks + instruction injection).
    SetPlanMode { enabled: bool },
    /// Persist whether a plan review card is pending (restore re-surfaces it).
    SetPlanReviewPending(bool),
    /// Persist the latest `UpdatePlan` snapshot (compaction survival: the
    /// transcript's plan tool calls are summarized away; the rail restores
    /// from the sidecar). `None` clears it (the model dropped its plan).
    PersistPlanSnapshot(Option<serde_json::Value>),
    /// Persist the mechanical first-message title before the first provider
    /// response (empty/image-only prompts leave the default title intact).
    SetInitialTitle(String),
    /// Persist an asynchronous Title-agent result through the session actor
    /// so it cannot race other sidecar updates owned by the actor.
    PersistGeneratedTitle {
        session_path: PathBuf,
        title: String,
    },
    /// Seed a fresh execution session with the approved plan as its active
    /// title source and wake the Title agent immediately.
    StartPlanExecution(String),
    /// A user-created/replaced Goal starts executing immediately.
    GoalStarted,
    /// Wake the goal gate now (user resumed an active goal while the agent
    /// was idle): queue the next continuation round if owed.
    GoalGate,
    /// Execute an approved plan: exit plan mode, optionally compact the
    /// planning context toward the plan file, then run the seed turn.
    ApprovePlan {
        compact: bool,
        compact_instructions: Option<String>,
        seed_text: String,
    },
    /// Re-point the session at an existing jsonl file.
    Open { path: PathBuf },
    /// Fork the current session into a worktree-cwd session and switch to
    /// it (`EnterWorktree` tool's git phase already ran).
    EnterWorktree {
        worktree_path: PathBuf,
        branch: String,
        original_cwd: PathBuf,
        git_common_dir: PathBuf,
    },
    /// Return to the pre-worktree session (`ExitWorktree` tool's git
    /// cleanup already ran).
    ExitWorktree,
    /// Create a fresh session in the given directory, optionally bound to a
    /// project (persisted in the session sidecar).
    NewSession {
        cwd: PathBuf,
        project: Option<PathBuf>,
    },
    /// Append a host UI annotation as a `custom` entry at the session leaf.
    /// The single actor queue makes send order the persist order, so a note
    /// dispatched before a prompt lands before the prompt's user entry.
    AppendUiNote(UiNoteRecord),
    /// Close the session and stop the actor.
    Shutdown,
}

// BackendNotice is the shared facade/backend contract (thread_engine.rs);
// the actor sends it over the notice channel the facade drains.

/// Authoritative state the actor writes and the facade mirrors.
struct EngineState {
    running: AtomicBool,
    history: Mutex<Vec<HistoryEntry>>,
    /// UI annotation cards with their position in the entry sequence; the
    /// live mirror re-merges them on every tick (the live transcript carries
    /// no custom entries). `append_custom` is the only other writer.
    notes: Mutex<Vec<PositionedNote>>,
    /// Bumped on every note append / authoritative sync so live ticks can
    /// skip re-merge churn when nothing moved.
    notes_gen: AtomicU64,
    /// Mid-run `AppendUiNote`s park here (the run owns the session); the
    /// idle loop drains and persists them.
    pending_ui_notes: Mutex<Vec<UiNoteRecord>>,
    request_usage: Mutex<HashMap<String, TokenUsage>>,
    /// Usage of each model's most recent request; the context-budget
    /// numerator behind the env card's `used / window` rows.
    per_model_last_usage: Mutex<HashMap<String, TokenUsage>>,
    cumulative: Mutex<TokenUsage>,
    per_model: Mutex<HashMap<String, TokenUsage>>,
    /// USD cost aggregated from the kernel's rate-card pricing (#418 wire
    /// boundary costing); 0 until the session carries priced usage.
    cumulative_cost: Mutex<f64>,
    per_model_cost: Mutex<HashMap<String, f64>>,
    /// The actor's working model; `SetModel` mirrors here so session
    /// builds always read the latest choice.
    model: Arc<Mutex<Option<PiModel>>>,
    sessions: Mutex<Vec<ThreadSummary>>,
    active_path: Mutex<Option<PathBuf>>,
    /// A browser-suite toggle that arrived mid-run; the running turn owns the
    /// session, so the toggle parks here and lands right after settle.
    pending_browser_suite: Mutex<Option<(BrowserSuite, bool)>>,
    /// Session cmds that arrived mid-run but are not serviceable until the
    /// turn settles (e.g. `EnterWorktree`/`ExitWorktree`/`NewSession`/`Open` —
    /// they rebuild the session, which the running turn owns). drive_run
    /// parks them here instead of dropping; the idle loop drains and
    /// re-queues them so the post-settle cmd dispatch runs the handler.
    pending_session_cmds: Mutex<Vec<SessionCmd>>,
    /// Plan-mode state shared by the actor, the hooks, the gate, and the
    /// `ProposePlan` tool.
    plan: Arc<crate::plan_mode::PlanSessionState>,
    /// The host permission gate wrapping every mutating tool (mode +
    /// pending interaction round trips).
    gate: Arc<ApprovalGate>,
    /// Shared goal state with the thread facade; the goal tools read/write
    /// through it, `GoalChanged` rides the notice channel. `None` when the
    /// threads db is unavailable (goal features degrade off).
    goal_bridge: Option<Arc<crate::goal_tools::GoalBridge>>,
    /// Fencing bit: at most one goal round queued at a time. Set by the gate
    /// before `follow_up`, cleared at that round's settle. Relaxed ordering
    /// is safe: set and clear both happen on the actor's single thread, and
    /// the goal gate never runs concurrently with itself (see the `armed`
    /// rationale in `GoalBridge`).
    goal_continuation_reserved: AtomicBool,
    /// Identity of the reserved round, for the settle-time admission check.
    goal_continuation_round: Mutex<Option<crate::goal_driver::GoalRoundIdentity>>,
    /// Plugin `SessionStart` hook fires once per session lifetime, before
    /// the first user turn. Restored sessions arm it at Ready (they already
    /// "started"); Open/NewSession re-arm per session switch.
    session_start_fired: AtomicBool,
    /// Active git-worktree binding shared with the worktree tools (nest
    /// guard + exit routing); persisted in the session sidecar on swap.
    worktree: crate::worktree::WorktreeState,
}

/// Live transcript snapshot maintained by the session listener so the engine
/// can serve a current view of the in-flight turn without borrowing the
/// session (the run future owns `&mut AgentSession` for its lifetime).
/// `messages` accumulates completed messages; `streaming` holds the partial
/// assistant message being generated (kernel `streaming_message` parity) and
/// is replaced on `MessageStart`/`MessageUpdate`, sealed into `messages` on
/// `MessageEnd`.
#[derive(Default)]
struct LiveTranscript {
    messages: Vec<AgentMessage>,
    streaming: Option<AgentMessage>,
}

/// The pi harness backend behind the `Thread` facade.
pub struct PiEngine {
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    state: Arc<EngineState>,
    bus: Arc<crate::steer_bus::AgentBus>,
}

/// Spawn the pi actor and return the engine handle plus its notice receiver.
/// The facade drains the receiver on the gpui thread. `initial_path`, when
/// given, opens that session file instead of restoring the newest one.
#[allow(clippy::too_many_arguments)] // engine spawn: startup options stay explicit
pub fn spawn_engine(
    cwd: PathBuf,
    model: Option<PiModel>,
    sessions_dir: PathBuf,
    initial_path: Option<PathBuf>,
    fresh: bool,
    project: Option<PathBuf>,
    thread_id: String,
    goal_bridge: Option<Arc<crate::goal_tools::GoalBridge>>,
    parent_session: Option<String>,
) -> SpawnedEngine {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (notice_tx, notice_rx) = mpsc::unbounded_channel();
    // The Steer bus is engine-scoped, not session-scoped: spawned-member
    // and live-subagent tracking must survive session swaps so a user
    // cancel always reaches every derivative this thread spawned.
    let bus = crate::steer_bus::AgentBus::new(thread_id.clone(), notice_tx.clone());
    let model_slot = Arc::new(Mutex::new(model.clone()));
    let gate = Arc::new(ApprovalGate::new(
        notice_tx.clone(),
        Arc::clone(&model_slot),
    ));
    let state = Arc::new(EngineState {
        running: AtomicBool::new(false),
        history: Mutex::new(Vec::new()),
        notes: Mutex::new(Vec::new()),
        notes_gen: AtomicU64::new(0),
        pending_ui_notes: Mutex::new(Vec::new()),
        request_usage: Mutex::new(HashMap::new()),
        per_model_last_usage: Mutex::new(HashMap::new()),
        cumulative: Mutex::new(TokenUsage::default()),
        per_model: Mutex::new(HashMap::new()),
        cumulative_cost: Mutex::new(0.0),
        per_model_cost: Mutex::new(HashMap::new()),
        model: model_slot,
        sessions: Mutex::new(Vec::new()),
        active_path: Mutex::new(initial_path.clone()),
        pending_browser_suite: Mutex::new(None),
        pending_session_cmds: Mutex::new(Vec::new()),
        gate,
        plan: crate::plan_mode::PlanSessionState::new(),
        goal_bridge,
        goal_continuation_reserved: AtomicBool::new(false),
        goal_continuation_round: Mutex::new(None),
        session_start_fired: AtomicBool::new(false),
        worktree: crate::worktree::new_state(),
    });
    crate::runtime::handle().spawn(run_actor(
        cwd,
        model,
        sessions_dir,
        initial_path.clone(),
        fresh,
        project,
        cmd_tx.clone(),
        cmd_rx,
        notice_tx.clone(),
        Arc::clone(&state),
        thread_id,
        parent_session,
        Arc::clone(&bus),
    ));
    // Display-only streaming preview: while the actor's eager restore reads
    // the whole session file, stream its transcript into the mirrored history
    // in batches so the workspace paints the first messages early. The
    // authoritative `sync_history` at `Ready` replaces the preview.
    if let Some(path) = initial_path {
        spawn_history_preview(path, Arc::clone(&state), notice_tx);
    }
    SpawnedEngine {
        engine: Arc::new(PiEngine { cmd_tx, state, bus }),
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
            let msgs: Vec<HistoryEntry> = entries
                .iter()
                .flat_map(pi::session::session_entry_to_context_messages)
                .flat_map(|m| adapt::harness_messages_to_messages(std::slice::from_ref(&m)))
                .map(HistoryEntry::Message)
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

    fn history(&self) -> Vec<HistoryEntry> {
        self.state.history.lock().unwrap().clone()
    }

    fn append_ui_note(&self, record: UiNoteRecord) {
        let _ = self.cmd_tx.send(SessionCmd::AppendUiNote(record));
    }

    fn request_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.state.request_usage.lock().unwrap().clone()
    }

    fn per_model_last_request_usage(&self) -> HashMap<String, TokenUsage> {
        self.state.per_model_last_usage.lock().unwrap().clone()
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
        if let Some(title) = crate::title::initial_title(&prompt) {
            let _ = self.cmd_tx.send(SessionCmd::SetInitialTitle(title));
        }
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

    fn abort_spawned_members(&self) {
        self.bus.abort_all_members();
    }

    fn set_model(&self, model: PiModel) {
        let _ = self.cmd_tx.send(SessionCmd::SetModel(model));
    }

    fn set_permission_mode(&self, mode: PermissionMode) {
        let _ = self.cmd_tx.send(SessionCmd::SetPermissionMode(mode));
    }

    fn set_plan_mode(&self, enabled: bool) {
        let _ = self.cmd_tx.send(SessionCmd::SetPlanMode { enabled });
    }

    fn set_browser_suite(&self, suite: BrowserSuite, enable: bool) {
        let _ = self
            .cmd_tx
            .send(SessionCmd::SetBrowserSuite { suite, enable });
    }

    fn set_plan_review_pending(&self, pending: bool) {
        let _ = self.cmd_tx.send(SessionCmd::SetPlanReviewPending(pending));
    }

    fn persist_plan_snapshot(&self, snapshot: Option<serde_json::Value>) {
        let _ = self.cmd_tx.send(SessionCmd::PersistPlanSnapshot(snapshot));
    }

    fn start_plan_execution(&self, plan_file: String) {
        let _ = self.cmd_tx.send(SessionCmd::StartPlanExecution(plan_file));
    }

    fn goal_started(&self) {
        let _ = self.cmd_tx.send(SessionCmd::GoalStarted);
    }

    /// Wake the goal gate (resume path): the actor queues the next
    /// continuation round when the agent is idle and the goal is armed.
    fn goal_gate(&self) {
        let _ = self.cmd_tx.send(SessionCmd::GoalGate);
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

/// Infer a capability tag for an agent definition from its declared tool
/// allowlist. The host snapshot carries Read/Grep/Glob/Ls (read),
/// Write/Edit (write), and Bash (exec); `tools: []` means the full
/// snapshot. The tag rides the `AgentToolDescription` template so the model
/// knows what each subagent can do before dispatching.
fn subagent_capability(def: &pi::ext_point_agent::AgentDef) -> &'static str {
    if def.tools.is_empty() {
        return "write+bash";
    }
    let has_write = def.tools.iter().any(|t| t == "Write" || t == "Edit");
    let has_bash = def.tools.iter().any(|t| t == "Bash");
    match (has_write, has_bash) {
        (true, true) => "write+bash",
        (true, false) => "write",
        (false, true) => "bash",
        (false, false) => "read-only",
    }
}

/// Host wrapper around `pi_extensions::bash::BashTool` for the subagent
/// snapshot. A Sailor is already an async primitive, so a background bash
/// inside a subagent session is pointless AND dangerous — the subagent's
/// registry has no manager/seatbelt-wrap, so `run_in_background` would
/// spawn a bare process that bypasses the seatbelt, eludes
/// `TaskStop`/`BashOutput`/UI, and outlives the session (no `Drop` reap).
/// Reject `run_in_background` outright; the description is also corrected
/// for the subagent context (one-shot, no state persistence, no gating
/// claim) since the kernel BashTool's static text assumes the host session.
struct SubagentBashTool {
    inner: Arc<dyn pi::tool::AgentTool>,
}

const SUBAGENT_BASH_DESCRIPTION: &str = "Execute a shell command. Each call runs in a fresh \
    one-shot shell at the current cwd (no persistent cwd/vars across calls), under the same \
    backend as the Captain (seatbelt-confined where the host has one). `run_in_background` is \
    not available inside a subagent — the subagent itself is the async primitive, so run long \
    commands in the foreground. Use `head_lines`/`tail_lines` to keep a selection of output \
    instead of piping through `head`/`tail`.";

#[async_trait::async_trait]
impl pi::tool::AgentTool for SubagentBashTool {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        SUBAGENT_BASH_DESCRIPTION
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        self.inner.requires_approval(params)
    }
    fn execution_mode(&self) -> pi::tool::ExecutionMode {
        // Delegate: the kernel BashTool declares Sequential (a stateful
        // persistent shell on non-macOS); the wrapper must inherit it so a
        // single Sailor session doesn't interleave parallel bash state.
        self.inner.execution_mode()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        // Strip `run_in_background` (refused by this wrapper — N1) and the
        // `sandbox_permissions`/`justification` escalation fields (no
        // escalation path in the ungated subagent session) so the model
        // never proposes them.
        let mut schema = self.inner.parameters_schema();
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.remove("run_in_background");
            props.remove("sandbox_permissions");
            props.remove("justification");
        }
        schema
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn pi::tool::ToolContext,
    ) -> Result<pi::tool::AgentToolResult, pi::tool::ToolError> {
        if params["run_in_background"].as_bool().unwrap_or(false) {
            return Err(pi::tool::ToolError::ExecutionFailed(
                "`run_in_background` is not available inside a subagent session — the subagent \
                 itself is the async primitive. Run the command in the foreground instead."
                    .into(),
            ));
        }
        self.inner.execute(tool_call_id, params, signal, ctx).await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn pi::tool::ToolContext,
        progress: &dyn pi::tool::ToolProgress,
    ) -> Result<pi::tool::AgentToolResult, pi::tool::ToolError> {
        if params["run_in_background"].as_bool().unwrap_or(false) {
            return Err(pi::tool::ToolError::ExecutionFailed(
                "`run_in_background` is not available inside a subagent session — the subagent \
                 itself is the async primitive. Run the command in the foreground instead."
                    .into(),
            ));
        }
        self.inner
            .execute_with_progress(tool_call_id, params, signal, ctx, progress)
            .await
    }
}

/// Host wrapper around `pi_extensions::bash::TaskStopTool` that also stops
/// legacy-registry tasks (asynchronously-dispatched Sailors). The kernel
/// `TaskStopTool` only knows the pi-extensions bash/monitor registries; a
/// Sailor registers in the legacy `background_task` registry, so the model
/// could not stop a runaway Sailor. This wrapper checks the legacy registry
/// first (calling `background_task::stop`, the same path the UI card uses),
/// and falls back to the kernel tool for bash/monitor/ws ids — one
/// `TaskStop` for every task kind.
struct LegacyAwareTaskStop {
    inner: Arc<TaskStopTool>,
}

const TASKSTOP_DESCRIPTION: &str = "Stop a background task by id — a background bash, a monitor, \
    or an asynchronously-dispatched Sailor subagent (`sailor_id`). Cancels the task's token; the \
    task settles to Stopped. Idempotent for an already-terminal task.";

#[async_trait::async_trait]
impl pi::tool::AgentTool for LegacyAwareTaskStop {
    fn name(&self) -> &str {
        "TaskStop"
    }
    fn description(&self) -> &str {
        TASKSTOP_DESCRIPTION
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        self.inner.requires_approval(params)
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn pi::tool::ToolContext,
    ) -> Result<pi::tool::AgentToolResult, pi::tool::ToolError> {
        // Legacy tasks (Sailors) live in background_task; stop there first.
        if let Some(id) = params["task_id"].as_str()
            && crate::background_task::get_by_str(id).is_some()
        {
            crate::background_task::stop(id)
                .await
                .map_err(pi::tool::ToolError::ExecutionFailed)?;
            return Ok(pi::tool::AgentToolResult::text(format!(
                "Stopped background task `{id}`"
            )));
        }
        // Else bash/monitor/ws — delegate to the kernel TaskStop.
        self.inner.execute(tool_call_id, params, signal, ctx).await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        ctx: &dyn pi::tool::ToolContext,
        _progress: &dyn pi::tool::ToolProgress,
    ) -> Result<pi::tool::AgentToolResult, pi::tool::ToolError> {
        // TaskStop never streams; route both entry points through execute.
        self.execute(tool_call_id, params, signal, ctx).await
    }
}

/// ChromeUse tool names — opt-in via the composer `+` menu, never in the
/// default active set, and never offered to subagents.
pub const CHROMEUSE_TOOL_NAMES: &[&str] = &[
    "ChromeUseOpen",
    "ChromeUseNavigate",
    "ChromeUseHover",
    "ChromeUseClick",
    "ChromeUseType",
    "ChromeUsePressKey",
    "ChromeUseSelectOption",
    "ChromeUseScroll",
    "ChromeUseSnapshot",
    "ChromeUseWaitFor",
    "ChromeUseScreenshot",
    "ChromeUseTabs",
    "ChromeUseEvaluate",
    "ChromeUseClose",
    "ChromeUseFindChromiumExecutable",
];
/// WebExplore (internal webview browser) tool names — opt-in, not default,
/// and never offered to subagents.
pub const WEBEXPLORE_TOOL_NAMES: &[&str] = &[
    "WebExploreOpen",
    "WebExploreNavigate",
    "WebExploreReadText",
    "WebExploreReadDom",
    "WebExploreClick",
    "WebExploreType",
    "WebExploreScroll",
    "WebExploreScreenshot",
    "WebExploreYield",
    "WebExploreClose",
];

/// The default active tool subset: every mounted tool except the browser
/// tool suites (ChromeUse + WebExplore), which stay dormant until the user
/// opts in via the composer `+` menu.
fn default_active_tool_names(tools: &[Arc<dyn PiAgentTool>]) -> Vec<String> {
    let browser: std::collections::HashSet<&str> = CHROMEUSE_TOOL_NAMES
        .iter()
        .chain(WEBEXPLORE_TOOL_NAMES)
        .copied()
        .collect();
    tools
        .iter()
        .map(|t| t.name().to_string())
        .filter(|name| !browser.contains(name.as_str()))
        .collect()
}

/// An opt-in browser tool suite toggled from the composer `+` menu. The
/// engine applies the toggle atomically against the session's authoritative
/// active-tool set, so callers never compute the merged set themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserSuite {
    ChromeUse,
    WebExplore,
}

impl BrowserSuite {
    /// The tool names belonging to this suite.
    pub fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::ChromeUse => CHROMEUSE_TOOL_NAMES,
            Self::WebExplore => WEBEXPLORE_TOOL_NAMES,
        }
    }
}
/// The full pi toolset: pi's file tools plus the pi-extensions bash/sub-agent
/// orchestration (assembly mirrors the `pi-extensions` orchestration example).
/// Every tool rides behind the host's [`ApprovalGatedTool`] (the kernel ships
/// no gate — permission policy is a harness concern); `AskUserQuestion` joins
/// ungated because asking the user is itself the interaction.
///
/// Returns the tools plus the session-scoped orchestrators that must attach
/// once the session exists (their steerers and lifecycle hooks need a live
/// session handle).
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
fn build_tools(
    cwd: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    cmd_tx: &mpsc::UnboundedSender<SessionCmd>,
    worktree: &crate::worktree::WorktreeState,
    bus: &Arc<crate::steer_bus::AgentBus>,
) -> (
    Vec<Arc<dyn PiAgentTool>>,
    SessionOrchestrators,
    crate::plan_mode::ReadOnlySubagentResolver,
) {
    // Bash execution backend: seatbelt-wrapped one-shot commands when the
    // OS backend is available (the per-call file-effect profile is rendered
    // from the effective `PermissionMode`; shell state does not persist —
    // the tool's `cwd` parameter pins each call), otherwise the unsandboxed
    // persistent brush shell (permission-gated as always). Background tasks
    // reuse this backend's `wrap_command`, so a non-escalated background
    // task is confined exactly like a foreground call. The per-call effective
    // mode reaches the seatbelt via the `mode_resolver`; a worktree session
    // scopes the workspace root to the worktree.
    let sandbox_available = crate::sandbox::is_available();
    let mut background = Arc::new(BackgroundRegistry::new());
    // Shared per-call grant cell: an approved sandbox-escalation stamps the
    // wider mode here for exactly one call; the sandboxed backend's mode
    // resolver reads it before the standing session mode.
    let grant_cell: Arc<std::sync::atomic::AtomicI64> = Arc::new(
        std::sync::atomic::AtomicI64::new(pi_extensions::sandbox::NO_GRANT),
    );
    let bash_ops: Arc<dyn pi::tools::bash::BashOperations> = if sandbox_available {
        let policy = worktree_policy(cwd, worktree);
        let sandbox_mode_gate = Arc::clone(gate);
        let cell_for_resolver = Arc::clone(&grant_cell);
        let sandbox_mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
            Arc::new(move || {
                let g = cell_for_resolver.load(std::sync::atomic::Ordering::SeqCst);
                if g != pi_extensions::sandbox::NO_GRANT {
                    PermissionMode::from_i64(g)
                } else {
                    sandbox_mode_gate.mode()
                }
            });
        let ops = Arc::new(crate::sandbox::SandboxedBashOperations::new(
            cwd,
            policy,
            sandbox_mode_resolver,
        ));
        let wrap_ops = Arc::clone(&ops);
        let wrap: pi_extensions::bash::background::SandboxCommandBuilder =
            Arc::new(move |command, cwd| wrap_ops.wrap_background(command, cwd));
        background = Arc::new(BackgroundRegistry::new().with_sandbox(wrap));
        ops
    } else {
        Arc::new(PersistentShellOperations::new(cwd))
    };
    // Subagent snapshot inherits the same seatbelt backend so a Sailor's
    // Bash is confined exactly like the Captain's (B4: no ungated bypass).
    let subagent_bash_ops: Arc<dyn pi::tools::bash::BashOperations> = Arc::clone(&bash_ops);
    let subagent_background = Arc::new(BackgroundRegistry::new());
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let monitor = Arc::new(MonitorManager::new(Arc::clone(&background)));
    // Unsandboxed backend (no confinement): selected per call when the
    // effective mode is `danger-full-access` (the standing session mode, or
    // an approved `sandbox_permissions` grant). Installed only where a
    // seatbelt exists to escape from; on other platforms the default backend
    // is already unsandboxed.
    let unsandboxed_ops: Option<Arc<dyn pi::tools::bash::BashOperations>> =
        sandbox_available.then(|| {
            let ops: Arc<dyn pi::tools::bash::BashOperations> =
                Arc::new(crate::sandbox::UnsandboxedBashOperations::new(cwd));
            ops
        });
    // Standing-mode resolver (the session mode, NOT the grant — the grant is
    // what an escalation requests, so it cannot be its own baseline).
    let standing_gate = Arc::clone(gate);
    let standing_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
        Arc::new(move || standing_gate.mode());
    let escalation_approver: Arc<dyn pi_extensions::sandbox::EscalationApprover + Send + Sync> =
        Arc::new(crate::pi_approval::GateEscalationApprover::new(Arc::clone(
            gate,
        )));
    let mut bash = BashTool::new(bash_ops, background.clone())
        .with_manager(Arc::clone(&manager))
        .with_sandbox_available(sandbox_available)
        .with_mode_resolver(Arc::clone(&standing_resolver))
        .with_grant_cell(grant_cell)
        .with_escalation_approver(Arc::clone(&escalation_approver));
    if let Some(ops) = unsandboxed_ops {
        bash = bash.with_unsandboxed_operations(ops);
    }
    let tools: Vec<Arc<dyn PiAgentTool>> = vec![
        // Read with oh-my-pi path selectors (`path:N-M` / `:raw` / multi-range);
        // selector-less reads delegate to the kernel ReadTool unchanged.
        Arc::new(pi_extensions::read::SelectorReadTool::new()),
        // Write/Edit carry the process write lock for their execution window:
        // concurrent writers to the same path get a named-holder conflict
        // instead of silently clobbering each other (old manox file_lock
        // semantics; owner stays "main" until the team system lands).
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(pi::tools::write::WriteTool),
            "main",
        )),
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(pi::tools::edit::EditTool),
            "main",
        )),
        Arc::new(pi::tools::grep::GrepTool),
        Arc::new(pi::tools::glob::GlobTool),
        Arc::new(pi::tools::ls::LsTool),
        Arc::new(bash),
        Arc::new(MonitorTool::new(Arc::clone(&monitor))),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(LegacyAwareTaskStop {
            inner: Arc::new(TaskStopTool::new(background).with_ws_registry(monitor.ws_registry())),
        }),
        Arc::new(crate::web_fetch::WebFetchTool::new()),
    ];
    // Plan-mode gate exemption: plan-file writes stay ungated while
    // plan mode is active (the `ToolCall` hook blocks everything else).
    let plan_policy = Arc::new(crate::plan_mode::PlanGatePolicy {
        state: Arc::clone(plan),
        plans_dir: crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans")),
        cwd: cwd.to_path_buf(),
    });
    // Bash rides the OS confinement (the per-call file-effect profile) instead
    // of the host gate whenever a seatbelt is mounted: the mode drives the
    // seatbelt directly, and a `sandbox_permissions` escalation is resolved
    // inside the tool through the host-injected approver (never the gate).
    // `Monitor`'s command half spawns through the same sandbox-wrapped
    // background registry under workspace-write.
    let confined_bash_auto_allow: Option<crate::pi_approval::AutoAllowResolver> = sandbox_available
        .then(|| {
            let allow: crate::pi_approval::AutoAllowResolver =
                Arc::new(move |name: &str, params: &serde_json::Value| match name {
                    "Bash" => true,
                    "Monitor" => params
                        .get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| !c.trim().is_empty()),
                    _ => false,
                });
            allow
        });
    // Write/Edit carry the host escalation config (shared approver + standing
    // resolver); the per-call grant is a local return value in the gate (no
    // shared cell — Write/Edit run `Parallel`). Every gated wrapper also
    // sees the live worktree-granted roots so an approved `EnterWorktree`
    // widens the fs fence exactly like the seatbelt.
    let wt_for_roots = Arc::clone(worktree);
    let worktree_roots: Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync> = Arc::new(move || {
        wt_for_roots
            .lock()
            .unwrap()
            .as_ref()
            .map(pi_extensions::sandbox::worktree_granted_roots)
            .unwrap_or_default()
    });
    let mut tools: Vec<Arc<dyn PiAgentTool>> = tools
        .into_iter()
        .map(|tool| {
            let name = tool.name().to_string();
            let mut wrapper = ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy))
                .with_worktree_roots(Arc::clone(&worktree_roots));
            if let Some(allow) = &confined_bash_auto_allow
                && matches!(name.as_str(), "Bash" | "Monitor")
            {
                wrapper = wrapper.with_auto_allow(Arc::clone(allow));
            }
            if matches!(name.as_str(), "Write" | "Edit") {
                wrapper = wrapper.with_escalation(
                    Arc::clone(&escalation_approver),
                    Arc::clone(&standing_resolver),
                );
            }
            Arc::new(wrapper) as Arc<dyn PiAgentTool>
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
    // Shared task-list tools: the team orchestration tools retired with the
    // Steer-based team architecture; the task list persists per thread and
    // every session registers the four tools. Gated like any other mutating
    // tool: ReadOnly denies, WorkspaceWrite admits (session-scoped, no
    // out-of-workspace target — see `workspace_write_verdict`), DangerFullAccess
    // runs ungated.
    let task_list = Arc::new(std::sync::Mutex::new(
        crate::team::tools::PlainTaskList::new(),
    ));
    for tool in [
        Arc::new(crate::team::tools::TaskCreateTool::with_list(Arc::clone(
            &task_list,
        ))) as Arc<dyn PiAgentTool>,
        Arc::new(crate::team::tools::TaskListTool::with_list(Arc::clone(
            &task_list,
        ))),
        Arc::new(crate::team::tools::TaskUpdateTool::with_list(Arc::clone(
            &task_list,
        ))),
        Arc::new(crate::team::tools::TaskGetTool::with_list(Arc::clone(
            &task_list,
        ))),
    ] {
        tools.push(Arc::new(
            ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy)),
        ));
    }
    // AskUserQuestion/ProposePlan — they persist the durable goal contract,
    // not filesystem side effects. Absent when the db is unavailable.
    if let Some(bridge) = goal_bridge {
        tools.push(Arc::new(crate::goal_tools::GetGoalTool::new(Arc::clone(
            bridge,
        ))));
        tools.push(Arc::new(crate::goal_tools::CreateGoalTool::new(
            Arc::clone(bridge),
        )));
        tools.push(Arc::new(crate::goal_tools::UpdateGoalTool::new(
            Arc::clone(bridge),
        )));
    }
    // Browser tools (main-thread host round trips via the facade): the read
    // axis stays ungated; the write axis rides the same permission gate as
    // built-ins. Plan mode's ToolCall hook blocks both (fixed allowlist).
    tools.push(Arc::new(crate::web_tools::WebExploreReadTextTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::web_tools::WebExploreReadDomTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::web_tools::WebExploreScreenshotTool::new(
        notice_tx.clone(),
    )));
    for tool in [
        Arc::new(crate::web_tools::WebExploreOpenTool::new(notice_tx.clone()))
            as Arc<dyn PiAgentTool>,
        Arc::new(crate::web_tools::WebExploreNavigateTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreClickTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreTypeTool::new(notice_tx.clone())),
        Arc::new(crate::web_tools::WebExploreScrollTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreYieldTool::new(
            notice_tx.clone(),
        )),
        Arc::new(crate::web_tools::WebExploreCloseTool::new(
            notice_tx.clone(),
        )),
    ] {
        tools.push(Arc::new(
            ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy)),
        ));
    }
    // ChromeUse (real Chrome via the in-process rustwright CDP engine): same
    // trust axes as WebExplore — reads stay ungated; writes ride the approval
    // gate. Plan mode's ToolCall hook blocks both (fixed allowlist). Compiled
    // only with the `chrome-use` feature: the VS Code host builds the agent
    // without it, so the engine is never linked into the extension.
    #[cfg(feature = "chrome-use")]
    {
        tools.push(Arc::new(crate::chrome_use::ChromeUseSnapshotTool));
        tools.push(Arc::new(crate::chrome_use::ChromeUseWaitForTool));
        tools.push(Arc::new(crate::chrome_use::ChromeUseScreenshotTool));
        tools.push(Arc::new(
            crate::chrome_use::ChromeUseFindChromiumExecutableTool,
        ));
        for tool in [
            Arc::new(crate::chrome_use::ChromeUseOpenTool) as Arc<dyn PiAgentTool>,
            Arc::new(crate::chrome_use::ChromeUseNavigateTool),
            Arc::new(crate::chrome_use::ChromeUseHoverTool),
            Arc::new(crate::chrome_use::ChromeUseClickTool),
            Arc::new(crate::chrome_use::ChromeUseTypeTool),
            Arc::new(crate::chrome_use::ChromeUsePressKeyTool),
            Arc::new(crate::chrome_use::ChromeUseSelectOptionTool),
            Arc::new(crate::chrome_use::ChromeUseScrollTool),
            Arc::new(crate::chrome_use::ChromeUseTabsTool),
            Arc::new(crate::chrome_use::ChromeUseEvaluateTool),
            Arc::new(crate::chrome_use::ChromeUseCloseTool),
        ] {
            tools.push(Arc::new(
                ApprovalGatedTool::new(tool, Arc::clone(gate))
                    .with_plan_policy(Arc::clone(&plan_policy)),
            ));
        }
    }
    // Git worktree management: the tools run the git phase and queue the
    // session swap (actor-side, between turns). Gated like other mutating
    // tools: repo-scoped, so WorkspaceWrite admits (see
    // `workspace_write_verdict`), ReadOnly denies, DangerFullAccess ungated.
    for tool in [
        Arc::new(crate::worktree::EnterWorktreeTool::new(
            cmd_tx.clone(),
            Arc::clone(worktree),
        )) as Arc<dyn PiAgentTool>,
        Arc::new(crate::worktree::ExitWorktreeTool::new(
            cmd_tx.clone(),
            Arc::clone(worktree),
        )),
    ] {
        tools.push(Arc::new(
            ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy)),
        ));
    }
    // LSP code-intel tools: read-only, ride ungated. Registered once the
    // registry probe landed and at least one server spec is available;
    // otherwise the agent degrades to grep/glob (no LSP on PATH).
    if let Some(reg) = lsp::registry::try_global()
        && !reg.available_specs().is_empty()
    {
        tools.extend(crate::lsp_tools::tools());
    }
    // MCP servers (mcp.toml + plugin .mcp.json): each advertised tool rides
    // behind the same permission gate as built-ins (remote calls are mutating
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
    // Steer bus: engine-scoped (created in `spawn_engine`), registered here
    // before the model-gated subagent block so the SteerTool is always
    // present (Dispatch returns an error until set_subagent_tool is called).
    tools.push(Arc::new(crate::steer_bus::SteerTool::new(
        Arc::clone(bus),
        pi_extensions::steer_bus::AgentId::Captain,
    )));
    // The watchdog's pull surface: the Captain queries live subagent health
    // (working / tool running / stalled / looping) before deciding to
    // Inject, Abort, or re-dispatch. Read-only; always live with the bus.
    tools.push(Arc::new(crate::steer_bus::SubagentStatusTool::new(
        Arc::clone(bus),
    )));
    // Plan-mode gate resolver: whether a subagent type is read-only. The
    // resolver shares the `AgentRegistry` Arc built below so the gate's
    // read-only notion can never diverge from the registry's capability
    // routing; it is model-independent and always live. The `SubagentTool`
    // itself needs a concrete model: wired eagerly when one is resolved at
    // assembly, otherwise on first Steer dispatch via the bus's late
    // configurator (resume-at-launch resolves the model shortly after).
    // Registry, plan-mode resolver, and sailor tool context are
    // model-independent: build them unconditionally so a session assembled
    // before a model resolved (resume-at-launch) still gets a live resolver.
    // The model-bound `SubagentTool` is wired eagerly when a model is present
    // and lazily on first Steer dispatch otherwise.
    let mut registry = AgentRegistry::new();
    register_defaults(&mut registry);
    // User-authored (~/.manox/agents) + plugin-provided
    // (`<plugin>/agents/`, namespaced) definitions layer over the
    // built-ins; same-name user files override built-ins.
    crate::agent_defs::register_user_and_plugin(&mut registry);
    // Render the Agent tool description against the live registry so the
    // model sees the available `subagent_type` values (Explore, Sailor,
    // user/plugin defs) with capability tags — no filesystem probing.
    // Tool descriptions are model-facing English, so always `Language::En`.
    let subagent_descriptions: Vec<crate::prompt::SubagentTypeData> = registry
        .all()
        .iter()
        .map(|def| crate::prompt::SubagentTypeData {
            name: def.name.clone(),
            capability: subagent_capability(def),
            description: def.description.clone(),
        })
        .collect();
    let registry = Arc::new(registry);
    let read_only_subagent: crate::plan_mode::ReadOnlySubagentResolver = {
        let r = Arc::clone(&registry);
        Arc::new(move |name: &str| {
            r.get(name)
                .map(subagent_capability)
                .is_some_and(|c| c == "read-only")
        })
    };
    let sailor_ctx: Arc<dyn pi::tool::ToolContext> = Arc::new(pi::tool::LocalToolContext::new(
        Arc::new(pi::env::TokioExecutionEnv::new(cwd.to_path_buf())),
        cwd.to_path_buf(),
        Arc::new(pi::tool::ToolState::new()),
    ));
    let subagent_description = match crate::prompt::render(
        crate::prompt::PromptTemplate::AgentToolDescription,
        crate::language::Language::En,
        &crate::prompt::AgentToolDescriptionData {
            subagents: subagent_descriptions,
        },
    ) {
        Ok(desc) => Some(desc),
        Err(e) => {
            tracing::warn!("Agent tool description render failed: {e}");
            None
        }
    };
    let model_slot = gate.model_slot();
    let build_subagent = {
        let registry = Arc::clone(&registry);
        let runtime = runtime.clone();
        let model_slot = Arc::clone(&model_slot);
        let provider_registry = crate::pi_providers::global();
        let subagent_bash_ops = Arc::clone(&subagent_bash_ops);
        let subagent_background = subagent_background.clone();
        let subagent_description = subagent_description.clone();
        move |model: &PiModel| {
            let subagent = SubagentTool::new(
                registry.clone(),
                vec![
                    Arc::new(pi_extensions::read::SelectorReadTool::new()),
                    Arc::new(pi::tools::grep::GrepTool),
                    Arc::new(pi::tools::glob::GlobTool),
                    Arc::new(pi::tools::ls::LsTool),
                    // Write/exec axis: definitions that opt into the full
                    // snapshot (e.g. Sailor, `tools: []`) get these; read-only
                    // definitions (Explore) name an explicit allowlist that
                    // `select_tools` filters against, so they never reach a
                    // read-only subagent. Bash inherits the Captain's seatbelt
                    // backend (no ungated bypass); Write/Edit carry the process
                    // write lock so parallel Sailors clobbering the same path
                    // surface a named-holder conflict instead of silently racing.
                    Arc::new(SubagentBashTool {
                        inner: Arc::new(
                            pi_extensions::bash::BashTool::new(
                                Arc::clone(&subagent_bash_ops),
                                subagent_background.clone(),
                            )
                            .with_sandbox_available(sandbox_available),
                        ),
                    }),
                    Arc::new(crate::file_lock::FileLockedTool::new(
                        Arc::new(pi::tools::write::WriteTool),
                        "sailor",
                    )),
                    Arc::new(crate::file_lock::FileLockedTool::new(
                        Arc::new(pi::tools::edit::EditTool),
                        "sailor",
                    )),
                ],
            )
            .with_model_runtime(runtime.clone())
            .with_model(model.clone())
            // Inherit the Captain's live model at dispatch time (a mid-thread
            // model switch is honored instead of falling back to the default).
            .with_model_slot(Arc::clone(&model_slot))
            // Resolve agent-definition `model` overrides against the live
            // registry (registration has landed before session assembly).
            .with_provider_registry(provider_registry.clone())
            // Subagent transcripts persist under the host session root (a
            // subdirectory the sidebar's non-recursive listing never
            // surfaces) so their usage stays accountable.
            .with_session_dir(crate::thread_store::sessions_dir().join("subagents"));
            let subagent = match subagent_description.clone() {
                Some(desc) => subagent.with_description(desc),
                None => subagent,
            };
            Arc::new(subagent)
        }
    };
    bus.set_tool_ctx(sailor_ctx);
    match model {
        Some(model) => bus.set_subagent_tool(build_subagent(model)),
        None => {
            // Launch-time resume assembles the session before the provider
            // catalog resolves a model; the model slot fills in shortly via
            // `SetModel`, so wire on first dispatch instead of dropping
            // subagent support for the session's lifetime. The configurator
            // returns the tool; the dispatch path caches it (writing back
            // into the bus from here would re-enter its locks).
            let configure: crate::steer_bus::LateConfigure = Arc::new(move || {
                let model = model_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()?;
                let tool = build_subagent(&model);
                tracing::info!("steer bus: subagent tool late-wired from live model slot");
                Some(tool)
            });
            bus.set_late_configure(configure);
        }
    }
    (
        tools,
        SessionOrchestrators {
            monitor,
            background: manager,
        },
        read_only_subagent,
    )
}

/// The sandbox policy for a session cwd: when the session runs inside the
/// active worktree, the worktree's granted roots (the worktree, its git
/// common dir, and the pre-enter project root) are admitted ON TOP of the
/// workspace — additive, so orchestration keeps writing the original repo.
/// The cwd comparison canonicalizes both sides so the policy follows the
/// session's actual directory — a stale pre-swap state or an Exit back to
/// the original repo both classify correctly.
fn worktree_policy(
    cwd: &Path,
    worktree: &crate::worktree::WorktreeState,
) -> crate::sandbox::SandboxPolicy {
    let Some(meta) = worktree.lock().unwrap().clone() else {
        return crate::sandbox::SandboxPolicy::for_project(cwd);
    };
    let wt = crate::sandbox::canonicalize_best_effort(Path::new(&meta.worktree_path));
    let current = crate::sandbox::canonicalize_best_effort(cwd);
    if current == wt {
        crate::sandbox::SandboxPolicy::for_project(cwd)
            .with_extra_roots(pi_extensions::sandbox::worktree_granted_roots(&meta))
    } else {
        crate::sandbox::SandboxPolicy::for_project(cwd)
    }
}
struct SessionOrchestrators {
    monitor: Arc<MonitorManager>,
    background: Arc<BackgroundManager>,
}

/// Bind the orchestrators to a freshly built session: the monitor steerer
/// lands events in the session's steering queue and the background manager
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

/// Merge a freshly appended UI note into the engine mirror at the tail and
/// record its position over the mapped-message count — the same base
/// `merge_positioned_notes` re-derives on live ticks.
fn mirror_ui_note(state: &Arc<EngineState>, record: UiNoteRecord) {
    let after_message = state
        .history
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| matches!(entry, HistoryEntry::Message(_)))
        .count();
    state
        .history
        .lock()
        .unwrap()
        .push(HistoryEntry::Note(record.clone()));
    state.notes.lock().unwrap().push(PositionedNote {
        note: record,
        after_message,
    });
    state.notes_gen.fetch_add(1, Ordering::SeqCst);
}

/// Serialize and append one UI note as a `custom` entry at the session leaf.
async fn persist_ui_note(session: &AgentSession, record: &UiNoteRecord) -> bool {
    let data = match serde_json::to_value(record) {
        Ok(value) => Some(value),
        Err(err) => {
            // A None payload renders as a ghost entry on reload; name the
            // impossible case loudly instead of silently dropping the card.
            tracing::warn!(error = %err, "UI note serialization failed");
            None
        }
    };
    if let Err(err) = session.append_custom(UI_NOTE_CUSTOM_TYPE, data).await {
        tracing::warn!(error = %err, "failed to persist UI note");
        false
    } else {
        true
    }
}

/// Merge or strip a browser suite's tool names into an active-tool set.
/// `enable` appends the suite's names (deduplicated); disable removes them.
/// Pure — unit-tested without a session.
fn toggle_browser_suite_names(
    mut names: Vec<String>,
    suite_names: &[&str],
    enable: bool,
) -> Vec<String> {
    if enable {
        for name in suite_names {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    } else {
        names.retain(|n| !suite_names.contains(&n.as_str()));
    }
    names
}

/// Toggle a browser tool suite against the session's authoritative active-tool
/// set. Reads the current set from the session, merges or strips the suite's
/// names, and persists via `set_active_tools`.
async fn apply_browser_suite(session: &mut AgentSession, suite: BrowserSuite, enable: bool) {
    // `None` means the full mounted set is active.
    let current = session
        .active_tool_names()
        .unwrap_or_else(|| session.tools());
    let names = toggle_browser_suite_names(current, suite.tool_names(), enable);
    if let Err(err) = session.set_active_tools(names).await {
        tracing::warn!(error = %err, "failed to toggle browser tool suite");
    }
}

/// The opt-in suites whose tool names are all present in a resolved
/// active-tool set; a suite is active only when fully selected.
fn project_browser_suites(active: &[String]) -> Vec<BrowserSuite> {
    [BrowserSuite::ChromeUse, BrowserSuite::WebExplore]
        .into_iter()
        .filter(|suite| {
            suite
                .tool_names()
                .iter()
                .all(|name| active.iter().any(|n| n == name))
        })
        .collect()
}

/// Drive one session run to completion while still servicing mid-run
/// commands (abort/steer/cancel/shutdown) through the session handle.
/// Shared by user prompts, monitor idle-wakeups, and plan-approval seeds.
/// Returns the run result and whether an abort was requested.
///
/// While the run is in flight, a periodic tick refreshes the engine's
/// history mirror from the live transcript (`LiveHistory` notice) so a
/// thread switched back to mid-turn rebuilds from current progress.
#[allow(clippy::too_many_arguments)] // drive plumbing: each input is a distinct sink
async fn drive_run<F>(
    run: F,
    handle: &pi::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    pi_model: &mut PiModel,
    sessions_dir: &Path,
    session_path: &Path,
) -> (anyhow::Result<Vec<AgentMessage>>, bool)
where
    F: std::future::Future<Output = anyhow::Result<Vec<AgentMessage>>>,
{
    tokio::pin!(run);
    let mut abort_requested = false;
    let mut channel_open = true;
    let mut live_ticker = tokio::time::interval(LIVE_HISTORY_TICK);
    live_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the mirror is not
    // re-synced at run start (the settle/ready path already mirrored).
    live_ticker.tick().await;
    let result = loop {
        if !channel_open {
            break run.await;
        }
        tokio::select! {
            _ = live_ticker.tick() => {
                if sync_live_history(&live, state) {
                    let _ = notice_tx.send(BackendNotice::LiveHistory);
                }
            }
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
                Some(SessionCmd::SetModel(new_model)) => {
                    // Mid-run switch: the harness handle applies it to the
                    // next provider request immediately and persists a
                    // model_change entry at the turn boundary (the kernel's
                    // TS mid-run setModel path). Mirror the shared slot and
                    // the actor's working model so settlement (title,
                    // session rebuilds) sees it too.
                    handle.set_model(new_model.clone());
                    *state.model.lock().unwrap() = Some(new_model.clone());
                    *pi_model = new_model;
                }
                Some(SessionCmd::SetThinkingLevel(level)) => {
                    // Mid-run switch: the queued mutation lands at the next
                    // turn boundary (same semantics as `set_model`); persist
                    // the choice so a reopened session restores it.
                    handle.set_thinking_level(level.clone());
                    if let Some(effort) = level.as_deref().and_then(parse_reasoning_effort)
                        && let Err(err) = write_reasoning_effort_sidecar(
                            sessions_dir,
                            session_path,
                            effort,
                        )
                        .await
                    {
                        tracing::warn!(error = %err, "failed to persist reasoning effort");
                    }
                }
                Some(SessionCmd::PersistGeneratedTitle {
                    session_path: target,
                    title,
                }) if target == session_path => {
                    if let Err(error) = persist_title(sessions_dir, &target, title).await {
                        tracing::warn!(%error, "failed to persist Title agent result");
                    } else {
                        let _ = notice_tx.send(BackendNotice::SessionListDirty);
                    }
                }
                Some(SessionCmd::AppendUiNote(record)) => {
                    // A mid-run switch-back must already show the card:
                    // merge it into the mirror now and park persistence for
                    // the idle loop (the run owns the `Session`; a second
                    // writer could fork the leaf cursor).
                    mirror_ui_note(state, record.clone());
                    state.pending_ui_notes.lock().unwrap().push(record);
                }
                Some(SessionCmd::SetBrowserSuite { suite, enable }) => {
                    // The run owns the session; park the toggle so the idle
                    // loop applies it right after settle (P2: a mid-run click
                    // must not be silently dropped).
                    *state.pending_browser_suite.lock().unwrap() = Some((suite, enable));
                }
                Some(cmd) => { // not serviceable mid-run; park for post-settle
                    state.pending_session_cmds.lock().unwrap().push(cmd);
                }
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
    sessions_dir: &Path,
    cwd: &Path,
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
    // Mid-run appends parked their persistence (the run owned the session);
    // persist before the authoritative sync so the rebuilt mirror keeps them.
    let parked = std::mem::take(&mut *state.pending_ui_notes.lock().unwrap());
    for record in parked {
        let _ = persist_ui_note(session, &record).await;
    }
    sync_history(session, sessions_dir, state).await;
    sync_usage(session, state).await;
    refresh_session_list(repo, state).await;
    let (steered, stranded) = if abort_requested || failed {
        (Vec::new(), std::mem::take(run_steers))
    } else {
        (std::mem::take(run_steers), Vec::new())
    };
    let _ = notice_tx.send(BackendNotice::Settled {
        cancelled: abort_requested,
        failed,
        steered,
        stranded,
    });
    // Plugin lifecycle: `Stop` fires on every settled turn (fail-open,
    // detached) — after the `Settled` notice so observers see the turn's
    // final state first.
    crate::plugin_hooks::fire(
        crate::plugin_hooks::HookEvent::Stop,
        cwd.to_str(),
        serde_json::json!({
            "cancelled": abort_requested,
            "failed": failed,
        }),
    );
}

/// Post-settle goal housekeeping shared by every run: disarm automatic
/// continuation on any cancellation or run error (DSH parity — the goal keeps
/// its durable phase until a human resume re-arms it), and admit the goal
/// round that just ran when one was in flight.
async fn goal_housekeeping(
    result: &anyhow::Result<Vec<AgentMessage>>,
    abort_requested: bool,
    session: &AgentSession,
    state: &Arc<EngineState>,
) {
    if (abort_requested || result.is_err())
        && let Some(bridge) = &state.goal_bridge
    {
        bridge.disarm();
    }
    if let Some(bridge) = &state.goal_bridge
        && bridge.goal_round_active()
    {
        let _ = crate::goal_driver::settle_goal_round(
            session,
            bridge,
            &state.goal_continuation_reserved,
            &state.goal_continuation_round,
            result,
            abort_requested,
        )
        .await;
    }
}

/// Chain automatic goal rounds from an idle position: gate first, run one
/// round, settle + housekeeping, repeat until no round is owed or the user
/// interrupts. The gate is consulted before every run, so a round is never
/// started on empty queues.
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
async fn chain_goal_rounds(
    session: &mut AgentSession,
    handle: &pi::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    pi_model: &mut PiModel,
    repo: &pi::session::repository::SessionRepository,
    sessions_dir: &Path,
    cwd: &Path,
    session_path: &Path,
) {
    loop {
        let queued = match &state.goal_bridge {
            Some(bridge) => {
                crate::goal_driver::maybe_queue_goal_round(
                    session,
                    bridge,
                    &state.goal_continuation_reserved,
                    &state.goal_continuation_round,
                    handle,
                )
                .await
            }
            None => return,
        };
        if !queued {
            return;
        }
        let (result, abort_requested) = drive_run(
            session.continue_(),
            handle,
            cmd_rx,
            run_steers,
            shutdown_after_run,
            Arc::clone(&live),
            state,
            notice_tx,
            pi_model,
            sessions_dir,
            session_path,
        )
        .await;
        settle_run(
            &result,
            abort_requested,
            session,
            state,
            repo,
            sessions_dir,
            cwd,
            notice_tx,
            run_steers,
        )
        .await;
        if *shutdown_after_run {
            return;
        }
        goal_housekeeping(&result, abort_requested, session, state).await;
    }
}

/// Forward every pi run event through the adapt mapping onto the notice
/// channel as UI events.
fn subscribe_session(
    session: &AgentSession,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    live: Arc<Mutex<LiveTranscript>>,
    title: TitleScheduler,
) -> pi::agent::Subscription {
    let event_tx = notice_tx.clone();
    // Seed the live mirror with the completed transcript so a mid-run tick
    // never drops restored history; the listener below appends from here.
    live.lock().unwrap().messages = session.harness_messages().to_vec();
    // A fresh session's jsonl file is deferred until the first assistant
    // message, so the sidebar only learns the thread exists once that file
    // materializes. The user MessageEnd fires before it; the first assistant
    // MessageEnd (the materialization moment — the persistence middleware
    // appends before listeners observe) is the authoritative signal.
    let assistant_signal_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let assistant_flag = std::sync::Arc::clone(&assistant_signal_sent);
    session.subscribe(Arc::new(move |event, _cancel| {
        let tx = event_tx.clone();
        let assistant_flag = std::sync::Arc::clone(&assistant_flag);
        let live = std::sync::Arc::clone(&live);
        let title = title.clone();
        Box::pin(async move {
            // Mirror the in-flight transcript for the live-history ticker:
            // completed messages accumulate, the streaming partial replaces
            // the slot until `MessageEnd` seals it.
            match &event {
                AgentEvent::MessageStart { message }
                | AgentEvent::MessageUpdate { message, .. } => {
                    live.lock().unwrap().streaming = Some((**message).clone());
                }
                AgentEvent::MessageEnd { message } => {
                    let mut guard = live.lock().unwrap();
                    guard.streaming = None;
                    guard.messages.push((**message).clone());
                }
                _ => {}
            }
            if let AgentEvent::MessageEnd { message } = &event {
                match &**message {
                    AgentMessage::User { .. } => {
                        let _ = tx.send(BackendNotice::SessionListDirty);
                    }
                    AgentMessage::Assistant { .. }
                        if !assistant_flag.swap(true, std::sync::atomic::Ordering::SeqCst) =>
                    {
                        // First assistant message: the deferred session file
                        // just materialized, so the sidebar can list it.
                        let _ = tx.send(BackendNotice::SessionListDirty);
                    }
                    _ => {}
                }
            }
            title.observe(&event);
            for te in adapt::agent_event_to_thread_events(&event) {
                let _ = tx.send(BackendNotice::Event(Box::new(te)));
            }
        })
    }))
}

/// Cheap change fingerprint for the live-history guard: message count plus
/// the trailing message's content size. Collisions only defer a facade
/// refresh by one tick, so exactness is unnecessary. Notes ride alongside
/// but never contribute: the `notes_gen` guard covers them.
fn live_fingerprint(mapped: &[HistoryEntry]) -> (usize, usize) {
    let trailing = mapped
        .iter()
        .filter_map(|e| match e {
            HistoryEntry::Message(m) => Some(m),
            _ => None,
        })
        .next_back()
        .map(|m| {
            m.content
                .iter()
                .map(|c| match c {
                    MessageContent::Text(t) => t.len(),
                    MessageContent::Thinking { text, .. } => text.len(),
                    MessageContent::Image { data, .. } => data.len(),
                    MessageContent::Compaction(t) => t.len(),
                    MessageContent::ToolUse(t) => t.name.len() + t.input.to_string().len(),
                    MessageContent::ToolResult(t) => t.content.len(),
                })
                .sum()
        })
        .unwrap_or(0);
    let count = mapped
        .iter()
        .filter(|e| matches!(e, HistoryEntry::Message(_)))
        .count();
    (count, trailing)
}

/// Refresh the engine's history mirror from the live transcript snapshot
/// (completed messages + the streaming partial). Returns whether the mirror
/// changed, so the caller can skip the facade notice on idle ticks (e.g. a
/// run parked on a user interaction where nothing is streaming).
fn sync_live_history(live: &Arc<Mutex<LiveTranscript>>, state: &Arc<EngineState>) -> bool {
    let mut msgs: Vec<AgentMessage> = Vec::new();
    {
        let guard = live.lock().unwrap();
        msgs.extend(guard.messages.iter().cloned());
        if let Some(streaming) = &guard.streaming {
            msgs.push(streaming.clone());
        }
    }
    let mut display: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&msgs)
        .into_iter()
        .map(HistoryEntry::Message)
        .collect();
    let notes_gen = state.notes_gen.load(Ordering::SeqCst);
    let mut history = state.history.lock().unwrap();
    if live_fingerprint(&history) == live_fingerprint(&display)
        && notes_gen == state.notes_gen.load(Ordering::SeqCst)
    {
        return false;
    }
    // The live transcript carries no custom entries: re-merge the positioned
    // notes so a mid-run switch-back keeps the cards at their position.
    let notes = state.notes.lock().unwrap().clone();
    merge_positioned_notes(&mut display, &notes);
    *history = display;
    true
}

/// Interleave notes right after their `after_message`-th message; a count of
/// zero lands at the top, an over-long count clamps to the tail. Notes are
/// stored in append order (non-decreasing positions), so sequential inserts
/// preserve their relative order.
fn merge_positioned_notes(display: &mut Vec<HistoryEntry>, notes: &[PositionedNote]) {
    for positioned in notes {
        let target = positioned.after_message;
        let mut insert_at = if target == 0 { 0 } else { display.len() };
        let mut count = 0usize;
        for (i, entry) in display.iter().enumerate() {
            if matches!(entry, HistoryEntry::Message(_)) {
                count += 1;
                if count == target {
                    insert_at = i + 1;
                }
            }
        }
        display.insert(insert_at, HistoryEntry::Note(positioned.note.clone()));
    }
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
            clear_user_chrome_spawn(sessions_dir.clone(), session_path.clone());
            let _ = tx.send(BackendNotice::Event(Box::new(ThreadEvent::Compaction {
                summary: result.summary,
                messages_compacted: 0,
                tokens_before: result.tokens_before,
            })));
        }
        _ => {}
    }))
}

/// Session header metadata: the creating host's identity, the owning thread
/// id (forks inherit it — `fork_from` copies the header metadata verbatim —
/// so a thread's sessions stay grouped across worktree swaps), plus (for a
/// team worker) the leader's session id. The links persist with the jsonl
/// file, so they survive restarts and outlive the in-memory team.
fn session_metadata(thread_id: &str, parent_session: Option<&str>) -> serde_json::Value {
    let mut metadata = serde_json::json!({
        "host": crate::host::current().slug(),
        "thread": thread_id,
    });
    if let Some(parent) = parent_session {
        metadata["team"] = serde_json::json!({ "parent": parent });
    }
    metadata
}

/// Build the session builder against the given project dir, using the shared
/// runtime and model.
#[allow(clippy::too_many_arguments)] // actor plumbing: each input is distinct session state
fn session_builder(
    cwd: &Path,
    sessions_dir: &Path,
    runtime: &ModelRuntime,
    model: Option<&PiModel>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    cmd_tx: &mpsc::UnboundedSender<SessionCmd>,
    worktree: &crate::worktree::WorktreeState,
    thread_id: &str,
    parent_session: Option<&str>,
    bus: &Arc<crate::steer_bus::AgentBus>,
) -> (
    pi::coding_agent::AgentSessionBuilder,
    SessionOrchestrators,
    crate::plan_mode::ReadOnlySubagentResolver,
) {
    let (tools, orchestrators, read_only_subagent) = build_tools(
        cwd,
        runtime,
        model,
        gate,
        plan,
        notice_tx,
        goal_bridge,
        cmd_tx,
        worktree,
        bus,
    );
    let default_active = default_active_tool_names(&tools);
    let mut builder = create_agent_session()
        .with_cwd(cwd.to_path_buf())
        .with_session_dir(sessions_dir.to_path_buf())
        .with_model_runtime(runtime.clone())
        .with_system_prompt_builder(pi_extensions::prompt::captain_prompt_builder(
            pi_extensions::prompt::CaptainConfig {
                cwd: cwd.to_path_buf(),
                today: chrono::Local::now().format("%Y-%m-%d").to_string(),
                skills: crate::skill::summaries_or_empty()
                    .into_iter()
                    .map(|s| pi_extensions::prompt::SkillSummary {
                        name: s.name,
                        description: s.description,
                    })
                    .collect(),
            },
        ))
        .with_resources(instruction_resources(cwd))
        .with_tools(tools)
        .with_initial_active_tools(default_active);
    // Persist hashline snapshots under the manox config dir so an edit tag
    // survives an app restart, a session fork, or a worktree re-entry.
    if let Ok(config_dir) = crate::paths::manox_config_dir() {
        builder = builder.with_snapshot_dir(config_dir.join("hashline-snapshots"));
    }
    // Every session a host creates is tagged with its identity so each
    // host's session list stays disjoint, and with its owning thread id so
    // the sidebar groups one thread's sessions (base + worktree forks) into
    // a single row. A team worker additionally carries its leader's session
    // id: the link persists with the jsonl file, so the affiliation survives
    // restarts and outlives the in-memory team.
    builder = builder.with_metadata(session_metadata(thread_id, parent_session));

    if let Some(model) = model {
        builder = builder.with_model(model.clone());
    }
    (builder, orchestrators, read_only_subagent)
}
/// Adopt the session's own model after a restore: the reopened session
/// projects its persisted model onto the harness, and the actor's working
/// model plus the shared slot must follow so `Ready` and the title
/// scheduler all see the restored choice.
fn adopt_session_model(session: &AgentSession, pi_model: &mut PiModel, state: &EngineState) {
    let restored = session.model().clone();
    *pi_model = restored.clone();
    *state.model.lock().unwrap() = Some(restored);
}

/// Register plan-mode extension hooks on a freshly built/restored session:
/// `BeforeAgentStart` injects the rendered plan-mode instructions every turn
/// while active; `ToolCall` enforces the read-only guarantee (plan-file
/// writes excepted). Both read through the shared [`PlanSessionState`].
fn attach_plan_hooks(
    session: &mut AgentSession,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    cwd: &Path,
    read_only_subagent: crate::plan_mode::ReadOnlySubagentResolver,
) {
    session.on(
        pi::harness::HookPoint::BeforeAgentStart,
        crate::plan_mode::injection_handler(Arc::clone(plan)),
    );
    let plans_dir = crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans"));
    session.on(
        pi::harness::HookPoint::ToolCall,
        crate::plan_mode::gate_handler(
            Arc::clone(plan),
            plans_dir,
            cwd.to_path_buf(),
            read_only_subagent,
        ),
    );
}

/// Instruction-file resources for the session: the Claude Code-compatible
/// memory hierarchy (managed policy, `~/.claude/CLAUDE.md` + rules, the
/// per-directory chain down to the session cwd) loaded through
/// [`crate::claude_md`] and folded into the system prompt by the kernel
/// every turn (TS project-instruction semantics). Skills/templates stay
/// empty here — manox skills ride the `agent::skill` registry instead.
fn instruction_resources(cwd: &Path) -> pi::harness::HarnessResources {
    let set = crate::claude_md::load(cwd, &crate::settings::claude_md_load_context());
    let context_files = set
        .eager
        .iter()
        .map(|src| pi::harness::ContextFile {
            name: src
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "CLAUDE.md".to_string()),
            location: src.path.display().to_string(),
            content: src.content.clone(),
        })
        .collect();
    pi::harness::HarnessResources {
        skills: Vec::new(),
        prompt_templates: Vec::new(),
        context_files,
    }
}

/// Register the plugin-lifecycle hook bridges (PreToolUse / PostToolUse
/// fire-and-forget shell-outs). Notification-only; never blocks a call.
fn attach_plugin_hooks(session: &mut AgentSession, cwd: &Path) {
    session.on(
        pi::harness::HookPoint::ToolCall,
        crate::plugin_hooks::pre_tool_call_handler(cwd.to_path_buf()),
    );
    session.on(
        pi::harness::HookPoint::ToolResult,
        crate::plugin_hooks::post_tool_result_handler(cwd.to_path_buf()),
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
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    state: Arc<EngineState>,
    thread_id: String,
    parent_session: Option<String>,
    bus: Arc<crate::steer_bus::AgentBus>,
) {
    // Session assembly preflights the model against the registry, so resolve
    // only after the one-shot background registration (parallelized per
    // provider, sub-second) has landed. The snapshot must be fetched AFTER
    // the wait: `global()` clones the current Arc, and the init thread
    // swaps it once registration completes — an early handle stays empty.
    crate::pi_providers::wait_ready().await;
    // Bound the LSP registry probe wait: a missing/slow probe must never
    // stall session assembly (tools register without LSP when it misses).
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::lsp_tools::wait_ready(),
    )
    .await;
    let registry = crate::pi_providers::global();
    let runtime = ModelRuntime::with_provider_registry(registry.clone()).with_catalog(Arc::new(
        crate::pi_providers::LegacyAliasCatalog::new(registry.clone()),
    ));
    // Goal tools emit `GoalChanged` through the notice channel once the
    // actor owns it (facade-side operations emit on the gpui thread).
    if let Some(bridge) = &state.goal_bridge {
        bridge.set_sender(notice_tx.clone());
    }
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
            // Only this host's sessions are eligible for restore; an explicit
            // path from another host is not restored (fail-closed).
            let mut list = list
                .into_iter()
                .filter(|info| crate::host::belongs_to_current_host(info.metadata.as_ref()));
            if let Some(requested) = &initial_path {
                return list.find(|info| info.path == *requested);
            }
            list.find(|info| info.message_count > 0)
        })
    };
    let mut restored = false;
    let mut session = None;
    // The restored worktree meta (if any) drives both the sandbox policy
    // (hydrated before the session build) and the post-Ready chip event.
    let mut worktree_restored: Option<pi_extensions::session_meta::WorktreeMeta> = None;
    if let Some(info) = latest {
        // Sessions created by a GUI launch (process cwd `/`) persisted a
        // useless cwd; heal them to this launch's default instead.
        let mut tool_cwd = PathBuf::from(info.cwd.clone());
        if tool_cwd.as_os_str() == "/" {
            tool_cwd = cwd.clone();
        }
        // Hydrate the worktree state BEFORE `session_builder` runs: it
        // calls `build_tools`, which derives the sandbox policy from the
        // active-worktree state (worktree-scoped confinement with the bound
        // repo's `.git` re-opened). Without this, a worktree session
        // restored after an app restart boots with the project policy and
        // commit/push fail until the next session swap.
        let meta = load_worktree_state(&sessions_dir, &info.path).await;
        worktree_restored = meta.clone();
        *state.worktree.lock().unwrap() = meta;
        // The restore path passes no model override: `builder.open()`
        // projects the session's own model only when the builder carries
        // none (TS `options.model > restored model`), and the actor adopts
        // it right after the open below.
        let (builder, orchestrators, read_only_subagent) = session_builder(
            &tool_cwd,
            &sessions_dir,
            &runtime,
            None,
            &state.gate,
            &state.plan,
            &notice_tx,
            state.goal_bridge.as_ref(),
            &cmd_tx,
            &state.worktree,
            &thread_id,
            None,
            &bus,
        );
        match builder.open(info.path).await {
            Ok(mut s) => {
                attach_orchestrators(&mut s, &orchestrators);
                crate::monitor_bridge::spawn(
                    Arc::clone(&orchestrators.monitor),
                    Arc::clone(&orchestrators.background),
                    notice_tx.clone(),
                    thread_id.clone(),
                );
                attach_plan_hooks(&mut s, &state.plan, &tool_cwd, read_only_subagent);
                attach_plugin_hooks(&mut s, &tool_cwd);
                adopt_session_model(&s, &mut pi_model, &state);
                restored = true;
                // The restored file is the thread's active session.
                crate::thread_registry::set_active(&thread_id, &info.id).await;
                session = Some(s);
            }
            Err(err) => {
                tracing::warn!("pi session restore failed ({err}); starting fresh");
                // The failed restore never materialized the worktree
                // session; clear the hydrated state so the fresh session
                // boots with the project policy.
                *state.worktree.lock().unwrap() = None;
            }
        }
    }
    let mut session = match session {
        Some(s) => s,
        None => {
            let (builder, orchestrators, read_only_subagent) = session_builder(
                &cwd,
                &sessions_dir,
                &runtime,
                Some(&pi_model),
                &state.gate,
                &state.plan,
                &notice_tx,
                state.goal_bridge.as_ref(),
                &cmd_tx,
                &state.worktree,
                &thread_id,
                parent_session.as_deref(),
                &bus,
            );
            // The fresh session carries the facade thread's id so the
            // sidebar row (keyed by session id) and the in-memory thread
            // share one identity.
            match builder.with_session_id(thread_id.clone()).build().await {
                Ok(mut s) => {
                    attach_orchestrators(&mut s, &orchestrators);
                    crate::monitor_bridge::spawn(
                        Arc::clone(&orchestrators.monitor),
                        Arc::clone(&orchestrators.background),
                        notice_tx.clone(),
                        thread_id.clone(),
                    );
                    attach_plan_hooks(&mut s, &state.plan, &cwd, read_only_subagent);
                    attach_plugin_hooks(&mut s, &cwd);
                    // A fresh session is pinned to the facade thread's id.
                    crate::thread_registry::set_active(&thread_id, &thread_id).await;
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
    // The live-transcript mirror the listener maintains and the run ticker
    // reads; shared so mid-run history refreshes never borrow the session.
    let live_mirror: Arc<Mutex<LiveTranscript>> = Arc::new(Mutex::new(LiveTranscript::default()));
    let restored_provider_responses = successful_provider_responses(session.harness_messages());
    let title_scheduler = TitleScheduler::new(
        runtime.clone(),
        Arc::clone(&state.model),
        Arc::clone(&live_mirror),
        state.goal_bridge.clone(),
        Arc::clone(&state.plan),
        session.path().to_path_buf(),
        cwd.clone(),
        cmd_tx.clone(),
        load_title_scheduler(&sessions_dir, session.path(), restored_provider_responses).await,
    );
    let mut _subscription = subscribe_session(
        &session,
        &notice_tx,
        Arc::clone(&live_mirror),
        title_scheduler.clone(),
    );
    let mut _harness_subscription = subscribe_harness_events(
        &mut session,
        sessions_dir.clone(),
        session_path,
        &notice_tx,
        &wakeup_tx,
    );

    // The permission mode rides the session sidecar: restore it so a
    // reopened session keeps its mode.
    let permission_mode = load_approval_mode(&sessions_dir, session.path()).await;
    state.gate.set_mode(permission_mode);
    // The reasoning effort rides the same sidecar: restore it so a reopened
    // Max session keeps its effort. The engine clamps against the current
    // model and persists the change in the transcript (TS `setThinkingLevel`).
    let reasoning_effort = load_reasoning_effort(&sessions_dir, session.path()).await;
    if reasoning_effort != ReasoningEffort::default()
        && let Err(err) = session
            .set_thinking_level(Some(reasoning_effort.wire_value().to_string()))
            .await
    {
        tracing::warn!(error = %err, "failed to restore reasoning effort");
    }
    // Plan mode rides the same sidecar: restore the flag so a reopened
    // planning session keeps its read-only gate; the facade re-renders and
    // re-sends the instructions once it sees `Ready`.
    let (plan_mode_restored, plan_file_restored) =
        load_plan_state(&sessions_dir, session.path()).await;
    state
        .plan
        .set(plan_mode_restored, plan_file_restored.clone());
    if plan_mode_restored {
        state
            .plan
            .set_active_instructions(render_plan_instructions());
    }
    let plan_review_pending = load_plan_review_pending(&sessions_dir, session.path()).await;
    let plan_snapshot = load_plan_snapshot(&sessions_dir, session.path()).await;

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
    // The active-tool set is authoritative for the composer chips; `None`
    // (never narrowed) reads as the full mounted set.
    let browser_suites = project_browser_suites(
        &session
            .active_tool_names()
            .unwrap_or_else(|| session.tools()),
    );
    let _ = notice_tx.send(BackendNotice::Ready(Box::new(ReadyInfo {
        restored,
        model: Some(pi_model.clone()),
        permission_mode,
        reasoning_effort,
        browser_suites,
        plan_mode: plan_mode_restored,
        plan_file: plan_file_restored,
        plan_review_pending,
        plan_snapshot,
    })));
    // A restored session already "started": arm the SessionStart hook latch
    // so the first prompt does not re-fire it.
    if restored {
        state.session_start_fired.store(true, Ordering::SeqCst);
    }
    // Restore the worktree chip after `Ready` so the facade mirror exists
    // before the event lands (same ordering as plan-mode restore).
    if let Some(meta) = worktree_restored {
        let _ = notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::WorktreeChanged {
                active: true,
                path: Some(meta.worktree_path),
            },
        )));
    }

    let mut run_steers: Vec<String> = Vec::new();
    let mut shutdown_after_run = false;

    loop {
        // Mid-run appends parked their persistence (the run owned the
        // session); drain before blocking on the next command.
        let parked = std::mem::take(&mut *state.pending_ui_notes.lock().unwrap());
        for record in parked {
            let _ = persist_ui_note(&session, &record).await;
        }
        // A mid-run browser-suite toggle parked itself for the same reason;
        // apply it now that the session is idle again. The guard is dropped
        // before the await so the future stays `Send`.
        let parked_suite = state.pending_browser_suite.lock().unwrap().take();
        if let Some((suite, enable)) = parked_suite {
            apply_browser_suite(&mut session, suite, enable).await;
        }
        // Mid-run non-serviceable cmds (EnterWorktree/ExitWorktree/...) parked
        // themselves for the same reason; re-queue so the post-settle dispatch
        // runs their handler.
        let parked_cmds = std::mem::take(&mut *state.pending_session_cmds.lock().unwrap());
        for cmd in parked_cmds {
            let _ = cmd_tx.send(cmd);
        }
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
                    // `continue_` drains the steering queue first. The
                    // facade learns the run started (`TurnStarted` sets its
                    // running flag) so a switch-away parks the thread instead
                    // of dropping it mid-run.
                    state.running.store(true, Ordering::Relaxed);
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::TurnStarted,
                    )));
                    let handle = session.handle();
                    let active_session_path = session.path().clone();
                    // One resume run for the steered events, then chain
                    // automatic goal rounds until the goal stops or the user
                    // interrupts (the gate re-checks after every settle).
                    let (result, abort_requested) = drive_run(
                        session.continue_(),
                        &handle,
                        &mut cmd_rx,
                        &mut run_steers,
                        &mut shutdown_after_run,
                        Arc::clone(&live_mirror),
                        &state,
                        &notice_tx,
                        &mut pi_model,
                        &sessions_dir,
                        &active_session_path,
                    )
                    .await;
                    settle_run(
                        &result,
                        abort_requested,
                        &session,
                        &state,
                        &repo,
                        &sessions_dir,
                        &cwd,
                        &notice_tx,
                        &mut run_steers,
                    )
                    .await;
                    if shutdown_after_run {
                        break;
                    }
                    goal_housekeeping(&result, abort_requested, &session, &state).await;
                    if shutdown_after_run {
                        break;
                    }
                    chain_goal_rounds(
                        &mut session,
                        &handle,
                        &mut cmd_rx,
                        &mut run_steers,
                        &mut shutdown_after_run,
                        Arc::clone(&live_mirror),
                        &state,
                        &notice_tx,
                        &mut pi_model,
                        &repo,
                        &sessions_dir,
                        &cwd,
                        &active_session_path,
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
                // Plugin lifecycle: `SessionStart` fires once per session,
                // before the first user turn (fail-open, detached).
                if !state.session_start_fired.swap(true, Ordering::SeqCst) {
                    crate::plugin_hooks::fire(
                        crate::plugin_hooks::HookEvent::SessionStart,
                        cwd.to_str(),
                        serde_json::json!({ "cwd": cwd.display().to_string() }),
                    );
                }
                state.running.store(true, Ordering::Relaxed);
                let handle = session.handle();
                let active_session_path = session.path().clone();
                // Drive the run while still servicing mid-run commands
                // (abort/steer) through the session handle, then chain
                // automatic goal rounds until the goal stops or the user
                // interrupts.
                let (result, abort_requested) = drive_run(
                    session.prompt_with_images(&text, images),
                    &handle,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                    Arc::clone(&live_mirror),
                    &state,
                    &notice_tx,
                    &mut pi_model,
                    &sessions_dir,
                    &active_session_path,
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
                    &repo,
                    &sessions_dir,
                    &cwd,
                    &notice_tx,
                    &mut run_steers,
                )
                .await;
                if shutdown_after_run {
                    break;
                }
                goal_housekeeping(&result, abort_requested, &session, &state).await;
                if shutdown_after_run {
                    break;
                }
                chain_goal_rounds(
                    &mut session,
                    &handle,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                    Arc::clone(&live_mirror),
                    &state,
                    &notice_tx,
                    &mut pi_model,
                    &repo,
                    &sessions_dir,
                    &cwd,
                    &active_session_path,
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
            SessionCmd::SetPermissionMode(mode) => {
                state.gate.set_mode(mode);
                if let Err(err) =
                    write_approval_mode_sidecar(&sessions_dir, session.path(), mode).await
                {
                    tracing::warn!(error = %err, "failed to persist approval mode");
                }
            }
            SessionCmd::SetBrowserSuite { suite, enable } => {
                apply_browser_suite(&mut session, suite, enable).await;
            }
            SessionCmd::SetPlanMode { enabled } => {
                let plan_file = enabled.then(|| state.plan.plan_file()).flatten();
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
                    write_plan_sidecar(&sessions_dir, session.path(), &state.plan).await
                {
                    tracing::warn!(error = %err, "failed to persist proposed plan source");
                }
                if let Err(err) =
                    write_plan_review_pending_sidecar(&sessions_dir, session.path(), pending).await
                {
                    tracing::warn!(error = %err, "failed to persist plan review pending flag");
                }
            }
            SessionCmd::PersistPlanSnapshot(snapshot) => {
                let completed = snapshot
                    .as_ref()
                    .and_then(|value| {
                        serde_json::from_value::<crate::plan::PlanSnapshot>(value.clone()).ok()
                    })
                    .is_some_and(|plan| plan.is_empty() || plan.all_completed());
                if snapshot.is_none() || completed {
                    state.plan.set_plan_file(None);
                    if let Err(err) =
                        write_plan_sidecar(&sessions_dir, session.path(), &state.plan).await
                    {
                        tracing::warn!(error = %err, "failed to retire completed plan title source");
                    }
                }
                if let Err(err) =
                    write_plan_snapshot_sidecar(&sessions_dir, session.path(), snapshot).await
                {
                    tracing::warn!(error = %err, "failed to persist plan snapshot");
                }
            }
            SessionCmd::SetInitialTitle(title) => {
                let should_write = {
                    let mut scheduler = title_scheduler.state.lock().unwrap();
                    if scheduler.title.is_none() {
                        scheduler.title = Some(title.clone());
                        true
                    } else {
                        false
                    }
                };
                if should_write {
                    if let Err(error) = persist_title(&sessions_dir, session.path(), title).await {
                        tracing::warn!(%error, "failed to persist initial title");
                    } else {
                        let _ = notice_tx.send(BackendNotice::SessionListDirty);
                    }
                }
            }
            SessionCmd::PersistGeneratedTitle {
                session_path,
                title,
            } => {
                if session.path() == &session_path {
                    if let Err(error) = persist_title(&sessions_dir, &session_path, title).await {
                        tracing::warn!(%error, "failed to persist Title agent result");
                    } else {
                        let _ = notice_tx.send(BackendNotice::SessionListDirty);
                    }
                }
            }
            SessionCmd::StartPlanExecution(plan_file) => {
                state.plan.set(false, Some(plan_file));
                if let Err(error) =
                    write_plan_sidecar(&sessions_dir, session.path(), &state.plan).await
                {
                    tracing::warn!(%error, "failed to persist plan execution title source");
                }
                title_scheduler.start_execution(crate::title::TitleWakeReason::PlanStarted);
            }
            SessionCmd::GoalStarted => {
                title_scheduler.start_execution(crate::title::TitleWakeReason::GoalStarted);
            }
            SessionCmd::GoalGate => {
                // Resume path: a goal was re-armed while the agent was idle.
                // Chain continuation rounds only when the gate actually owes
                // one — no round, no run (continue_ with empty queues errors).
                if !state.running.load(Ordering::Relaxed) {
                    state.running.store(true, Ordering::Relaxed);
                    let _ =
                        notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)));
                    let handle = session.handle();
                    let active_session_path = session.path().clone();
                    chain_goal_rounds(
                        &mut session,
                        &handle,
                        &mut cmd_rx,
                        &mut run_steers,
                        &mut shutdown_after_run,
                        Arc::clone(&live_mirror),
                        &state,
                        &notice_tx,
                        &mut pi_model,
                        &repo,
                        &sessions_dir,
                        &cwd,
                        &active_session_path,
                    )
                    .await;
                    if shutdown_after_run {
                        break;
                    }
                }
                // Running: the in-flight run's settle path already chains
                // rounds; the gate fence prevents a double queue.
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
                title_scheduler.start_execution(crate::title::TitleWakeReason::PlanStarted);
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
                let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)));
                let handle = session.handle();
                let active_session_path = session.path().clone();
                let (result, abort_requested) = drive_run(
                    session.prompt(&seed_text),
                    &handle,
                    &mut cmd_rx,
                    &mut run_steers,
                    &mut shutdown_after_run,
                    Arc::clone(&live_mirror),
                    &state,
                    &notice_tx,
                    &mut pi_model,
                    &sessions_dir,
                    &active_session_path,
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
                    &repo,
                    &sessions_dir,
                    &cwd,
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
                        clear_user_chrome(&sessions_dir, session.path()).await;
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
                if let Err(err) = session.set_thinking_level(level.clone()).await {
                    tracing::warn!("pi set_thinking_level failed: {err}");
                }
                // The facade's knob only sends "high"/"max"; persist the
                // choice so a reopened session restores the same effort.
                if let Some(effort) = level.as_deref().and_then(parse_reasoning_effort)
                    && let Err(err) =
                        write_reasoning_effort_sidecar(&sessions_dir, session.path(), effort).await
                {
                    tracing::warn!(error = %err, "failed to persist reasoning effort");
                }
            }
            SessionCmd::Open { path } => {
                rebuild_session(
                    &mut session,
                    &path,
                    &sessions_dir,
                    &runtime,
                    &mut pi_model,
                    &state,
                    &cwd,
                    &notice_tx,
                    &state.gate,
                    &state.plan,
                    state.goal_bridge.as_ref(),
                    &cmd_tx,
                    &state.worktree,
                    &thread_id,
                    &bus,
                )
                .await;
                title_scheduler.retarget(
                    path.clone(),
                    cwd.clone(),
                    load_title_scheduler(
                        &sessions_dir,
                        &path,
                        successful_provider_responses(session.harness_messages()),
                    )
                    .await,
                );
                _subscription = subscribe_session(
                    &session,
                    &notice_tx,
                    Arc::clone(&live_mirror),
                    title_scheduler.clone(),
                );
                _harness_subscription = subscribe_harness_events(
                    &mut session,
                    sessions_dir.to_path_buf(),
                    path.to_path_buf(),
                    &notice_tx,
                    &wakeup_tx,
                );
                resync_plan_state(&sessions_dir, &path, &state.plan, &notice_tx).await;
                resync_worktree_state(&sessions_dir, &path, &state.worktree, &notice_tx).await;
                // The opened file becomes the thread's active session.
                let opened_id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                crate::thread_registry::set_active(&thread_id, opened_id).await;
                *state.active_path.lock().unwrap() = Some(path);
                resync_approval_mode(&session, &sessions_dir, &state, &notice_tx).await;
                // Opened sessions are resumed conversations: SessionStart
                // already happened in a prior lifetime.
                state.session_start_fired.store(true, Ordering::SeqCst);
                sync_history(&session, &sessions_dir, &state).await;
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
            }
            SessionCmd::EnterWorktree {
                worktree_path,
                branch,
                original_cwd,
                git_common_dir,
            } => {
                let Some(current_path) = state.active_path.lock().unwrap().clone() else {
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                        anyhow::anyhow!("enter_worktree: no active session"),
                    ))));
                    continue;
                };
                // Kernel-native `forkFrom`: the forked session carries the
                // transcript verbatim with the worktree as its header cwd.
                let fork_path = match repo
                    .fork_from(&current_path, &worktree_path.display().to_string())
                    .await
                {
                    Ok(forked) => forked.storage().path().to_path_buf(),
                    Err(err) => {
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                            anyhow::anyhow!("enter_worktree fork failed: {err:#}"),
                        ))));
                        continue;
                    }
                };
                // The fork becomes the thread's active session; the thread
                // id stamp rides the forked header metadata, so the sidebar
                // keeps both sessions under one row.
                let fork_id = fork_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                crate::thread_registry::set_active(&thread_id, fork_id).await;
                // The worktree sidecar + shared state must be in place
                // BEFORE the session rebuild: `build_tools` derives the
                // sandbox policy from the active-worktree state
                // (worktree-scoped confinement plus the owning repo's git
                // common dir admitted). The WorktreeChanged notice fires
                // early here; the final refresh_session_list repaints
                // everything.
                let meta = pi_extensions::session_meta::WorktreeMeta {
                    worktree_path: worktree_path.display().to_string(),
                    branch,
                    original_session_path: current_path.display().to_string(),
                    original_cwd: original_cwd.display().to_string(),
                    git_common_dir: git_common_dir.display().to_string(),
                };
                if let Err(err) =
                    fork_sidecar_inherit(&sessions_dir, &current_path, &fork_path, meta).await
                {
                    tracing::warn!(error = %err, "failed to persist the worktree fork sidecar");
                }
                resync_worktree_state(&sessions_dir, &fork_path, &state.worktree, &notice_tx).await;
                rebuild_session(
                    &mut session,
                    &fork_path,
                    &sessions_dir,
                    &runtime,
                    &mut pi_model,
                    &state,
                    &cwd,
                    &notice_tx,
                    &state.gate,
                    &state.plan,
                    state.goal_bridge.as_ref(),
                    &cmd_tx,
                    &state.worktree,
                    &thread_id,
                    &bus,
                )
                .await;
                title_scheduler.retarget(
                    fork_path.clone(),
                    worktree_path.clone(),
                    load_title_scheduler(
                        &sessions_dir,
                        &fork_path,
                        successful_provider_responses(session.harness_messages()),
                    )
                    .await,
                );
                _subscription = subscribe_session(
                    &session,
                    &notice_tx,
                    Arc::clone(&live_mirror),
                    title_scheduler.clone(),
                );
                _harness_subscription = subscribe_harness_events(
                    &mut session,
                    sessions_dir.to_path_buf(),
                    fork_path.clone(),
                    &notice_tx,
                    &wakeup_tx,
                );
                resync_plan_state(&sessions_dir, &fork_path, &state.plan, &notice_tx).await;
                *state.active_path.lock().unwrap() = Some(fork_path);
                resync_approval_mode(&session, &sessions_dir, &state, &notice_tx).await;
                sync_history(&session, &sessions_dir, &state).await;
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
            }
            SessionCmd::ExitWorktree => {
                let Some(meta) = state.worktree.lock().unwrap().clone() else {
                    continue;
                };
                let original = PathBuf::from(&meta.original_session_path);
                rebuild_session(
                    &mut session,
                    &original,
                    &sessions_dir,
                    &runtime,
                    &mut pi_model,
                    &state,
                    &cwd,
                    &notice_tx,
                    &state.gate,
                    &state.plan,
                    state.goal_bridge.as_ref(),
                    &cmd_tx,
                    &state.worktree,
                    &thread_id,
                    &bus,
                )
                .await;
                title_scheduler.retarget(
                    original.clone(),
                    PathBuf::from(&meta.original_cwd),
                    load_title_scheduler(
                        &sessions_dir,
                        &original,
                        successful_provider_responses(session.harness_messages()),
                    )
                    .await,
                );
                _subscription = subscribe_session(
                    &session,
                    &notice_tx,
                    Arc::clone(&live_mirror),
                    title_scheduler.clone(),
                );
                _harness_subscription = subscribe_harness_events(
                    &mut session,
                    sessions_dir.to_path_buf(),
                    original.clone(),
                    &notice_tx,
                    &wakeup_tx,
                );
                resync_plan_state(&sessions_dir, &original, &state.plan, &notice_tx).await;
                if let Err(err) = write_worktree_sidecar(&sessions_dir, &original, None).await {
                    tracing::warn!(error = %err, "failed to clear worktree sidecar");
                }
                resync_worktree_state(&sessions_dir, &original, &state.worktree, &notice_tx).await;
                // The pointer returns to the pre-worktree session.
                let original_id = original
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                crate::thread_registry::set_active(&thread_id, &original_id).await;
                *state.active_path.lock().unwrap() = Some(original);
                resync_approval_mode(&session, &sessions_dir, &state, &notice_tx).await;
                sync_history(&session, &sessions_dir, &state).await;
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
            }
            SessionCmd::NewSession { cwd, project } => {
                let (builder, orchestrators, read_only_subagent) = session_builder(
                    &cwd,
                    &sessions_dir,
                    &runtime,
                    Some(&pi_model),
                    &state.gate,
                    &state.plan,
                    &notice_tx,
                    state.goal_bridge.as_ref(),
                    &cmd_tx,
                    &state.worktree,
                    &thread_id,
                    parent_session.as_deref(),
                    &bus,
                );
                // Same identity contract as the startup build: the session
                // carries the facade thread's id (the previous deferred
                // session never materialized — `set_project` requires a
                // non-interacted thread).
                match builder.with_session_id(thread_id.clone()).build().await {
                    Ok(mut s) => {
                        attach_orchestrators(&mut s, &orchestrators);
                        crate::monitor_bridge::spawn(
                            Arc::clone(&orchestrators.monitor),
                            Arc::clone(&orchestrators.background),
                            notice_tx.clone(),
                            thread_id.clone(),
                        );
                        attach_plan_hooks(&mut s, &state.plan, &cwd, read_only_subagent);
                        attach_plugin_hooks(&mut s, &cwd);
                        // A fresh session never inherits plan mode — clear
                        // any state left over from the previous session.
                        state.plan.set(false, None);
                        state.plan.set_active_instructions(None);
                        // …and earns its own SessionStart on the first turn.
                        state.session_start_fired.store(false, Ordering::SeqCst);
                        // …nor an active worktree binding.
                        *state.worktree.lock().unwrap() = None;
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::WorktreeChanged {
                                active: false,
                                path: None,
                            },
                        )));
                        session = s;
                        // The fresh session is pinned to the facade thread's id.
                        crate::thread_registry::set_active(&thread_id, &thread_id).await;
                        let new_path = session.path().to_path_buf();
                        title_scheduler.retarget(
                            new_path.clone(),
                            cwd.clone(),
                            load_title_scheduler(
                                &sessions_dir,
                                &new_path,
                                successful_provider_responses(session.harness_messages()),
                            )
                            .await,
                        );
                        _subscription = subscribe_session(
                            &session,
                            &notice_tx,
                            Arc::clone(&live_mirror),
                            title_scheduler.clone(),
                        );
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
            SessionCmd::AppendUiNote(record) => {
                // Persist at the leaf through the append queue and refresh
                // the mirror so an idle switch-away sees the note before the
                // next authoritative sync.
                if persist_ui_note(&session, &record).await {
                    mirror_ui_note(&state, record);
                }
            }
            SessionCmd::Shutdown => break,
        }
    }

    let _ = session.close().await;
    // Thread-lifetime cleanup: cancel (SessionEnded) every task this thread
    // owns — including in-flight asynchronously-dispatched Sailors — then
    // release the legacy registry entries + mailbox. `cleanup_thread` alone
    // only `retain`s (drops entries without cancelling tokens); the cancel
    // must run first or a deleted thread's Sailors become unfindable zombies
    // still burning tokens.
    crate::background_task::cancel_all_for_thread(&thread_id).await;
    crate::background_task::cleanup_thread(&thread_id);
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
    pi_model: &mut PiModel,
    state: &EngineState,
    fallback_cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    cmd_tx: &mpsc::UnboundedSender<SessionCmd>,
    worktree: &crate::worktree::WorktreeState,
    thread_id: &str,
    bus: &Arc<crate::steer_bus::AgentBus>,
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
    // Like the startup restore, a session swap passes no model override so
    // the opened session's own persisted model wins (TS `options.model >
    // restored model`); the actor adopts it right after the open.
    let (builder, orchestrators, read_only_subagent) = session_builder(
        &cwd,
        sessions_dir,
        runtime,
        None,
        gate,
        plan,
        notice_tx,
        goal_bridge,
        cmd_tx,
        worktree,
        thread_id,
        None,
        bus,
    );
    match builder.open(path.to_path_buf()).await {
        Ok(mut s) => {
            attach_orchestrators(&mut s, &orchestrators);
            crate::monitor_bridge::spawn(
                Arc::clone(&orchestrators.monitor),
                Arc::clone(&orchestrators.background),
                notice_tx.clone(),
                thread_id.to_string(),
            );
            attach_plan_hooks(&mut s, plan, &cwd, read_only_subagent);
            // Session swaps (Open/EnterWorktree/ExitWorktree) must carry the
            // same plugin lifecycle hooks as fresh builds, or the swapped
            // session's PreToolUse/PostToolUse fire-and-forget shell-outs
            // never attach (write confinement is now in ApprovalGatedTool).
            attach_plugin_hooks(&mut s, &cwd);
            adopt_session_model(&s, pi_model, state);
            *session = s;
        }
        Err(err) => {
            let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                "pi session open failed: {err}"
            )));
        }
    }
}

#[derive(Debug, Default)]
struct TitleSchedulerState {
    title: Option<String>,
    in_flight: bool,
    rerun: Option<crate::title::TitleWakeReason>,
    wake: crate::title::TitleWakeState,
    generation: u64,
}

impl TitleSchedulerState {
    fn begin(&mut self, reason: crate::title::TitleWakeReason) -> bool {
        if self.in_flight {
            self.rerun = Some(reason);
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    fn finish(&mut self) -> Option<crate::title::TitleWakeReason> {
        self.in_flight = false;
        self.rerun.take()
    }
}

#[derive(Debug, Default)]
struct PersistedTitleScheduler {
    title: Option<String>,
    provider_responses: usize,
}

#[derive(Clone)]
struct TitleScheduler {
    state: Arc<Mutex<TitleSchedulerState>>,
    runtime: ModelRuntime,
    model: Arc<Mutex<Option<PiModel>>>,
    live: Arc<Mutex<LiveTranscript>>,
    goal: Option<Arc<crate::goal_tools::GoalBridge>>,
    plan: Arc<crate::plan_mode::PlanSessionState>,
    session_path: Arc<Mutex<PathBuf>>,
    cwd: Arc<Mutex<PathBuf>>,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
}

impl TitleScheduler {
    #[allow(clippy::too_many_arguments)]
    fn new(
        runtime: ModelRuntime,
        model: Arc<Mutex<Option<PiModel>>>,
        live: Arc<Mutex<LiveTranscript>>,
        goal: Option<Arc<crate::goal_tools::GoalBridge>>,
        plan: Arc<crate::plan_mode::PlanSessionState>,
        session_path: PathBuf,
        cwd: PathBuf,
        cmd_tx: mpsc::UnboundedSender<SessionCmd>,
        persisted: PersistedTitleScheduler,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(TitleSchedulerState {
                title: persisted.title,
                wake: crate::title::TitleWakeState::restore(persisted.provider_responses, false),
                ..Default::default()
            })),
            runtime,
            model,
            live,
            goal,
            plan,
            session_path: Arc::new(Mutex::new(session_path)),
            cwd: Arc::new(Mutex::new(cwd)),
            cmd_tx,
        }
    }

    fn retarget(&self, session_path: PathBuf, cwd: PathBuf, persisted: PersistedTitleScheduler) {
        *self.session_path.lock().unwrap() = session_path;
        *self.cwd.lock().unwrap() = cwd;
        let generation = self.state.lock().unwrap().generation.wrapping_add(1);
        *self.state.lock().unwrap() = TitleSchedulerState {
            title: persisted.title,
            wake: crate::title::TitleWakeState::restore(persisted.provider_responses, false),
            generation,
            ..Default::default()
        };
    }

    fn observe(&self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart => {
                self.state.lock().unwrap().wake.turn_start();
            }
            AgentEvent::MessageEnd { message } => {
                let AgentMessage::Assistant { stop_reason, .. } = &**message else {
                    return;
                };
                if matches!(
                    stop_reason,
                    Some(pi::types::StopReason::Error | pi::types::StopReason::Aborted)
                ) {
                    return;
                }
                let reasons = self
                    .state
                    .lock()
                    .unwrap()
                    .wake
                    .assistant_response(*stop_reason);
                for reason in reasons {
                    self.wake(reason);
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } => {
                if tool_name == crate::tools::CREATE_GOAL && !is_error {
                    self.start_execution(crate::title::TitleWakeReason::GoalStarted);
                } else if result.terminate {
                    // `wake` re-locks the scheduler state, so the guard from
                    // the scrutinee must drop first: an `if let` scrutinee
                    // guard stays alive through the block and would
                    // self-deadlock the non-reentrant std mutex.
                    let reason = self.state.lock().unwrap().wake.tool_terminated();
                    if let Some(reason) = reason {
                        self.wake(reason);
                    }
                }
            }
            _ => {}
        }
    }

    fn start_execution(&self, reason: crate::title::TitleWakeReason) {
        self.state.lock().unwrap().wake.start_execution();
        self.wake(reason);
    }

    fn wake(&self, reason: crate::title::TitleWakeReason) {
        if !crate::settings::side_calls().title_policy().enabled {
            return;
        }
        {
            let mut state = self.state.lock().unwrap();
            if !state.begin(reason) {
                return;
            }
        }
        let generation = self.state.lock().unwrap().generation;
        let session_path = self.session_path.lock().unwrap().clone();
        let scheduler = self.clone();
        crate::runtime::handle().spawn(async move {
            scheduler.run(reason, generation, session_path).await;
        });
    }

    async fn run(
        self,
        reason: crate::title::TitleWakeReason,
        generation: u64,
        session_path: PathBuf,
    ) {
        let current = self.state.lock().unwrap().title.clone();
        let messages = self.live.lock().unwrap().messages.clone();
        let goal = self.goal.as_ref().and_then(|bridge| bridge.snapshot());
        let source =
            crate::title::select_source(goal.as_ref(), self.plan.plan_file().as_deref(), &messages);
        let request = crate::title::TitleRequest {
            source,
            current_title: current.clone(),
            reason,
        };
        let model = self.model.lock().unwrap().clone();
        let cwd = self.cwd.lock().unwrap().clone();
        let result = match model {
            Some(model) => {
                crate::title::run_title_agent(&self.runtime, &model, &cwd, &request).await
            }
            None => Ok(None),
        };
        let next = {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation {
                return;
            }
            if let Ok(Some(title)) = &result
                && state.title.as_deref() != Some(title)
            {
                state.title = Some(title.clone());
            }
            state.finish()
        };
        match result {
            Ok(Some(title)) if current.as_deref() != Some(title.as_str()) => {
                let _ = self.cmd_tx.send(SessionCmd::PersistGeneratedTitle {
                    session_path,
                    title,
                });
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, ?reason, "Title agent failed; keeping current title")
            }
        }
        if let Some(reason) = next {
            self.wake(reason);
        }
    }
}

#[cfg(test)]
mod title_scheduler_tests {
    use super::*;
    use crate::title::TitleWakeReason;

    #[test]
    fn in_flight_wakes_coalesce_to_the_latest_reason() {
        let mut state = TitleSchedulerState::default();
        assert!(state.begin(TitleWakeReason::FirstResponse));
        assert!(!state.begin(TitleWakeReason::Stop));
        assert!(!state.begin(TitleWakeReason::GoalStarted));
        assert_eq!(state.finish(), Some(TitleWakeReason::GoalStarted));
        assert!(!state.in_flight);
    }

    #[test]
    fn tool_terminate_observe_releases_state_guard_before_wake() {
        // Regression: the `if let` scrutinee guard from `state.lock()` stays
        // alive through the block, so a `wake` call inside it self-deadlocked
        // the non-reentrant std mutex when a terminate tool ended the turn.
        let (tx, _rx) = mpsc::unbounded_channel::<SessionCmd>();
        let resolver: pi::agent_loop::StreamResolver = Arc::new(
            |_model: &PiModel| -> Result<Arc<dyn pi::agent_loop::StreamFn>, anyhow::Error> {
                Err(anyhow::anyhow!("unused in this test"))
            },
        );
        let scheduler = TitleScheduler::new(
            ModelRuntime::new(resolver),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(LiveTranscript::default())),
            None,
            crate::plan_mode::PlanSessionState::new(),
            PathBuf::new(),
            PathBuf::new(),
            tx,
            PersistedTitleScheduler::default(),
        );
        // Park `wake` at the begin gate so the fixed code returns without
        // spawning a title run; the buggy code deadlocks on the re-lock
        // before it ever reaches that gate.
        scheduler.state.lock().unwrap().in_flight = true;
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: crate::plan_mode::PROPOSE_PLAN.into(),
            result: pi::tool::AgentToolResult {
                content: vec![ContentBlock::Text {
                    text: "plan submitted".into(),
                    signature: None,
                }],
                details: None,
                is_error: false,
                usage: None,
                added_tool_names: None,
                terminate: true,
            },
            is_error: false,
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            scheduler.observe(&event);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("observe deadlocked on a terminate tool result");
    }
}

async fn load_title_scheduler(
    sessions_dir: &Path,
    session_path: &Path,
    provider_responses: usize,
) -> PersistedTitleScheduler {
    pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .ok()
        .map(|meta| PersistedTitleScheduler {
            title: meta.title.filter(|title| !title.trim().is_empty()),
            provider_responses,
        })
        .unwrap_or_default()
}

fn successful_provider_responses(messages: &[AgentMessage]) -> usize {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                AgentMessage::Assistant {
                    stop_reason,
                    error_message: None,
                    ..
                } if !matches!(
                    stop_reason,
                    Some(pi::types::StopReason::Error | pi::types::StopReason::Aborted)
                )
            )
        })
        .count()
}

async fn persist_title(
    sessions_dir: &Path,
    session_path: &Path,
    title: String,
) -> Result<(), anyhow::Error> {
    // Probe first (unlocked read): persisting an unchanged title would only
    // churn the sidecar; the locked re-read inside `update` makes any race
    // benign (title is last-write-wins either way).
    let current = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    if current.title.as_deref() == Some(title.as_str()) {
        return Ok(());
    }
    pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.title = Some(title.clone());
    })
    .await
}

/// The permission mode persisted in a session's sidecar (wire field
/// `approval_mode`); fresh sessions (missing sidecar or field) and unknown
/// values land on the bounded default.
async fn load_approval_mode(sessions_dir: &Path, session_path: &Path) -> PermissionMode {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => meta
            .approval_mode
            .as_deref()
            .and_then(|raw| serde_json::from_value(serde_json::Value::String(raw.to_string())).ok())
            .unwrap_or_default(),
        Err(_) => PermissionMode::default(),
    }
}

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
    pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.plan_mode = plan.enabled().then_some(true);
        meta.plan_file = plan.plan_file();
    })
    .await
}

async fn load_plan_review_pending(sessions_dir: &Path, session_path: &Path) -> bool {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => meta.plan_review_pending.unwrap_or(false),
        Err(_) => false,
    }
}

/// The last `UpdatePlan` snapshot persisted in the sidecar (compaction
/// survival: the transcript's plan tool calls are summarized away, but the
/// rail's plan restores from here).
async fn load_plan_snapshot(sessions_dir: &Path, session_path: &Path) -> Option<serde_json::Value> {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => meta.plan_snapshot,
        Err(_) => None,
    }
}

/// The active worktree binding persisted in a session's sidecar (forked
/// worktree sessions carry it; originals and plain sessions don't).
async fn load_worktree_state(
    sessions_dir: &Path,
    session_path: &Path,
) -> Option<pi_extensions::session_meta::WorktreeMeta> {
    pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .ok()
        .and_then(|meta| meta.worktree)
}

/// Persist (or clear) the worktree binding in the session sidecar.
async fn write_worktree_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    worktree: Option<pi_extensions::session_meta::WorktreeMeta>,
) -> Result<(), anyhow::Error> {
    pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.worktree = worktree;
    })
    .await
}

/// Seed a worktree fork's sidecar: the worktree binding plus the source
/// session's title and project, which the fork wears as the thread's
/// identity while it is the active session (thread view title, sidebar
/// grouping). The fork's sidecar is fresh — it carries no identity of its
/// own, so the source's is adopted unconditionally. A missing/unreadable
/// source sidecar degrades to the binding alone.
async fn fork_sidecar_inherit(
    sessions_dir: &Path,
    source_path: &Path,
    fork_path: &Path,
    worktree: pi_extensions::session_meta::WorktreeMeta,
) -> Result<(), anyhow::Error> {
    let source = pi_extensions::session_meta::load(sessions_dir, source_path)
        .await
        .ok();
    pi_extensions::session_meta::update(sessions_dir, fork_path, |meta| {
        if let Some(src) = &source {
            meta.title = src.title.clone();
            meta.project = src.project.clone();
        }
        meta.worktree = Some(worktree);
    })
    .await
}

/// Re-sync the active-worktree state after a session switch: the binding
/// follows the opened session's sidecar. Emits `WorktreeChanged` so the
/// facade mirror tracks the session it now mirrors.
async fn resync_worktree_state(
    sessions_dir: &Path,
    session_path: &Path,
    worktree: &crate::worktree::WorktreeState,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let meta = load_worktree_state(sessions_dir, session_path).await;
    let active = meta.is_some();
    let path = meta.as_ref().map(|m| m.worktree_path.clone());
    *worktree.lock().unwrap() = meta;
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::WorktreeChanged { active, path },
    )));
}

async fn write_plan_review_pending_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    pending: bool,
) -> Result<(), anyhow::Error> {
    pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.plan_review_pending = pending.then_some(true);
    })
    .await
}

async fn write_plan_snapshot_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    snapshot: Option<serde_json::Value>,
) -> Result<(), anyhow::Error> {
    pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.plan_snapshot = snapshot;
    })
    .await
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

/// Persist the permission mode in the session sidecar (wire field
/// `approval_mode`, kebab values) so the session reopens with the same
/// gate policy.
async fn write_approval_mode_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    mode: PermissionMode,
) -> Result<(), anyhow::Error> {
    let raw = serde_json::to_value(mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("PermissionMode serializes to its kebab wire name");
    pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.approval_mode = Some(raw);
    })
    .await
}

/// The reasoning effort persisted in a session's sidecar; fresh sessions
/// (missing sidecar or field) default to High.
async fn load_reasoning_effort(sessions_dir: &Path, session_path: &Path) -> ReasoningEffort {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => match meta.reasoning_effort.as_deref() {
            Some("high") => ReasoningEffort::High,
            Some("max") => ReasoningEffort::Max,
            _ => ReasoningEffort::default(),
        },
        Err(_) => ReasoningEffort::default(),
    }
}

/// Persist the reasoning effort in the session sidecar so the session
/// reopens with the same effort.
async fn write_reasoning_effort_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    effort: ReasoningEffort,
) -> Result<(), anyhow::Error> {
    pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.reasoning_effort = Some(effort.wire_value().to_string());
    })
    .await
}

/// Parse the facade's effort knob wire value; other levels (e.g. "off")
/// are not user-facing and yield `None`.
fn parse_reasoning_effort(raw: &str) -> Option<ReasoningEffort> {
    match raw {
        "high" => Some(ReasoningEffort::High),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

/// Re-read the session's persisted permission mode after a session switch
/// and align the gate + the facade's chip with it.
async fn resync_approval_mode(
    session: &AgentSession,
    sessions_dir: &Path,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let mode = load_approval_mode(sessions_dir, session.path()).await;
    state.gate.set_mode(mode);
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::PermissionModeChanged { mode },
    )));
}

/// Mirror the session's authoritative entry list (compaction-aware, every
/// entry type) into engine state: the display sequence interleaves UI note
/// entries at their persisted position, then re-attaches the sidecar's
/// per-ordinal user-turn chrome (registry slash display forms + agent
/// attributions) — the transcript stores only wire content, so the attach
/// is what keeps a reloaded thread's bubbles send-time accurate.
async fn sync_history(session: &AgentSession, sessions_dir: &Path, state: &Arc<EngineState>) {
    let entries = match session.context_entries().await {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(error = %err, "context entries unavailable; mirror left as-is");
            return;
        }
    };
    let (mut display, notes) = adapt::entries_to_display(&entries);
    attach_registry_displays(
        &mut display,
        &load_registry_displays(sessions_dir, session.path()).await,
    );
    attach_user_attributions(
        &mut display,
        &load_user_attributions(sessions_dir, session.path()).await,
    );
    *state.history.lock().unwrap() = display;
    *state.notes.lock().unwrap() = notes;
    state.notes_gen.fetch_add(1, Ordering::SeqCst);
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

/// The agent attributions persisted per user-message ordinal by
/// `Thread::persist_user_attribution`. Missing sidecar or field reads as
/// empty (human-only transcript).
async fn load_user_attributions(
    sessions_dir: &Path,
    session_path: &Path,
) -> std::collections::HashMap<usize, pi_extensions::session_meta::UserAttributionMeta> {
    pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .map(|meta| meta.user_attributions)
        .unwrap_or_default()
}

/// Drop the per-ordinal user-turn chrome (registry display forms + agent
/// attributions). A compaction rebuilds the transcript as a summary user
/// message plus the retained tail, which shifts every persisted ordinal;
/// the stale records would otherwise attach to the wrong user prompt on
/// the next `sync_history`. New turns after the compaction persist fresh
/// ordinals over the rebuilt sequence.
async fn clear_user_chrome(sessions_dir: &Path, session_path: &Path) {
    // Probe first: clearing empty maps would only churn the sidecar.
    let meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    if meta.registry_displays.is_empty() && meta.user_attributions.is_empty() {
        return;
    }
    if let Err(err) = pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.registry_displays.clear();
        meta.user_attributions.clear();
    })
    .await
    {
        tracing::warn!(error = %err, "failed to clear user turn chrome");
    }
}

/// `clear_user_chrome` from a harness event listener, which is
/// synchronous — the clear runs on the runtime and its outcome is only
/// display chrome, so a lost write just narrows the reload window.
fn clear_user_chrome_spawn(sessions_dir: PathBuf, session_path: PathBuf) {
    crate::runtime::handle().spawn(async move {
        clear_user_chrome(&sessions_dir, &session_path).await;
    });
}

/// Re-attach `display_text` to the user prompt message at each persisted
/// ordinal. The ordinal counts `Role::User` messages with a `User`
/// provenance — the same set `Thread` counts when persisting — so steers
/// (user prompts too) and tool results (excluded) align between the two.
fn attach_registry_displays(
    history: &mut [HistoryEntry],
    displays: &std::collections::HashMap<usize, String>,
) {
    if displays.is_empty() {
        return;
    }
    let mut ordinal = 0usize;
    for entry in history {
        let HistoryEntry::Message(message) = entry else {
            continue;
        };
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

/// Re-attach `author`/`peer` to the user prompt message at each persisted
/// ordinal. The ordinal counts `Role::User` messages with a `User`
/// provenance — the same set `Thread` counts when persisting — so steers
/// (user prompts too) and tool results (excluded) align between the two.
fn attach_user_attributions(
    history: &mut [HistoryEntry],
    attributions: &std::collections::HashMap<
        usize,
        pi_extensions::session_meta::UserAttributionMeta,
    >,
) {
    if attributions.is_empty() {
        return;
    }
    let mut ordinal = 0usize;
    for entry in history {
        let HistoryEntry::Message(message) = entry else {
            continue;
        };
        if message.role == crate::language_model::Role::User
            && message.provenance == crate::message::MessageProvenance::User
        {
            if let Some(record) = attributions.get(&ordinal) {
                let ui = message.ui.get_or_insert_with(Default::default);
                ui.author = Some(crate::message::MessageAuthor::from_routing(&record.author));
                ui.peer = record.peer;
                ui.display_text = record.display_text.clone();
            }
            ordinal += 1;
        }
    }
}

/// Persist the bound project in the session sidecar so the sidebar groups
/// the session under its project folder across restarts.
async fn write_project_sidecar(sessions_dir: &Path, session_path: &Path, project: &Path) {
    if let Err(err) = pi_extensions::session_meta::update(sessions_dir, session_path, |meta| {
        meta.project = Some(project.to_string_lossy().to_string());
    })
    .await
    {
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
    store_attribution(state, session);
}

/// Rebuild both attribution maps from the authoritative mapped history and
/// the kernel transcript, then mirror them into engine state.
fn store_attribution(state: &Arc<EngineState>, session: &AgentSession) {
    let (per_turn, per_model_last) =
        request_attribution(&state.history.lock().unwrap(), session.harness_messages());
    *state.request_usage.lock().unwrap() = per_turn;
    *state.per_model_last_usage.lock().unwrap() = per_model_last;
}

/// Presentation-layer attribution for the env card, walked in lockstep over
/// the kernel transcript and the authoritative mapped history so per-turn
/// keys are the facade's own message ids. Returns the per-turn totals (each
/// assistant request accumulates under the user message that triggered its
/// turn) and each model's latest single request (the context-budget
/// numerator; summing requests would count the repeated prompt prefix once
/// per tool-loop iteration).
fn request_attribution(
    history: &[HistoryEntry],
    messages: &[AgentMessage],
) -> (HashMap<String, TokenUsage>, HashMap<String, TokenUsage>) {
    let mut per_turn: HashMap<String, TokenUsage> = HashMap::new();
    let mut per_model_last: HashMap<String, TokenUsage> = HashMap::new();
    let mut ids = history.iter().filter_map(|entry| match entry {
        HistoryEntry::Message(message) => Some(message),
        HistoryEntry::Note(_) => None,
    });
    let mut turn_key: Option<String> = None;
    let mut over_consumed = false;
    for m in messages {
        // Id-stream cardinality must mirror
        // `adapt::harness_messages_to_messages`: hidden Custom messages
        // produce no mapped row.
        let id = match m {
            AgentMessage::Custom {
                display, content, ..
            } if !*display || content.is_empty() => None,
            _ => match ids.next() {
                Some(id) => Some(id),
                None => {
                    over_consumed = true;
                    None
                }
            },
        };
        match m {
            AgentMessage::User { .. } => turn_key = id.map(|m| m.id.clone()),
            AgentMessage::Assistant {
                usage,
                model,
                provider,
                response_model,
                ..
            } => {
                let u = to_token_usage(usage);
                if u.total_tokens() == 0 {
                    continue;
                }
                if let Some(key) = &turn_key {
                    per_turn
                        .entry(key.clone())
                        .and_modify(|acc| *acc = *acc + u)
                        .or_insert(u);
                }
                let key = format!(
                    "{provider}/{}",
                    response_model.as_deref().unwrap_or(model.as_str())
                );
                per_model_last.insert(key, u);
            }
            _ => {}
        }
    }
    // Cardinality backstop: the skip rules above manually mirror
    // `adapt::harness_messages_to_messages`; a divergence shifts every later
    // attribution, so trip loudly in debug builds.
    debug_assert!(
        !over_consumed && ids.next().is_none(),
        "request_attribution id stream out of lockstep with the mapped history"
    );
    (per_turn, per_model_last)
}

/// Fallback aggregation when `session_stats()` is unavailable: assistant
/// usage only. Keys keep the stats path's "{provider}/{model}" shape so
/// consumers can resolve the model regardless of which path produced them.
fn sync_usage_from_messages(session: &AgentSession, state: &Arc<EngineState>) {
    let mut cumulative = TokenUsage::default();
    let mut per_model: HashMap<String, TokenUsage> = HashMap::new();
    for m in session.harness_messages() {
        let AgentMessage::Assistant {
            usage,
            model,
            provider,
            response_model,
            ..
        } = m
        else {
            continue;
        };
        let u = to_token_usage(usage);
        if u.total_tokens() == 0 {
            continue;
        }
        cumulative = cumulative + u;
        let key = format!(
            "{provider}/{}",
            response_model.as_deref().unwrap_or(model.as_str())
        );
        per_model
            .entry(key)
            .and_modify(|acc| *acc = *acc + u)
            .or_insert(u);
    }
    *state.cumulative.lock().unwrap() = cumulative;
    *state.per_model.lock().unwrap() = per_model;
    *state.cumulative_cost.lock().unwrap() = 0.0;
    *state.per_model_cost.lock().unwrap() = HashMap::new();
    store_attribution(state, session);
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
            // The mirrored list stays host-scoped like the sidebar's own.
            // Subagent transcripts persist for usage accounting but never
            // surface as threads.
            if !crate::host::belongs_to_current_host(info.metadata.as_ref())
                || info
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m.get("subagent").is_some())
            {
                continue;
            }
            out.push(session_info_to_summary(&info));
        }
    }
    *state.sessions.lock().unwrap() = out;
}

/// Map a pi session info onto the actor's mirrored session list.
///
/// A flat mirror of the sidebar store (the sidebar itself renders
/// `ThreadStore::summaries()` with team depth resolution); rows here stay
/// depth 0 because the mirror is never rendered as a tree. `parent_id`
/// still resolves the team affiliation via the shared helper so any reader
/// sees the same edge the sidebar does.
fn session_info_to_summary(info: &pi::session::repository::SessionInfo) -> ThreadSummary {
    ThreadSummary {
        id: info.id.clone(),
        summary: info.first_message.clone(),
        title: None,
        title_override: None,
        model_id: String::new(),
        provider_id: None,
        approval_mode: PermissionMode::default().as_i64(),
        project: if info.cwd == "/" {
            String::new()
        } else {
            info.cwd.clone()
        },
        depth: 0,
        parent_id: crate::thread_store::team_parent_id(info)
            .or_else(|| info.parent_session_path.clone()),
        archived: false,
        pinned: false,
        // The actor's mirror never reads the sidecar; tags stay sidebar-only.
        tag: None,
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
///
/// The harness<->pi translation is internal to the crate in production builds;
/// it is exposed as `pub` only under `test-support` so integration tests (e.g.
/// the agent-ui overlap walk) can rebuild a conversation from a captured
/// session without widening the production API surface.
#[cfg(feature = "test-support")]
#[path = "pi_engine_adapt.rs"]
pub mod adapt;
#[cfg(not(feature = "test-support"))]
#[path = "pi_engine_adapt.rs"]
pub(crate) mod adapt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_active_tool_names_excludes_browser_suites() {
        // The default active set keeps every non-browser tool and drops both
        // opt-in suites, so browser tools never ride the default system prompt.
        let tools: Vec<Arc<dyn PiAgentTool>> = vec![
            Arc::new(crate::chrome_use::ChromeUseOpenTool) as Arc<dyn PiAgentTool>,
            Arc::new(crate::web_tools::WebExploreOpenTool::new(
                tokio::sync::mpsc::unbounded_channel().0,
            )),
        ];
        let default_names = default_active_tool_names(&tools);
        assert!(default_names.is_empty(), "{default_names:?}");
    }

    #[test]
    fn toggle_browser_suite_enable_merges_into_default_set() {
        // P0 regression: activating a suite against the default set must ADD
        // the suite's tools, not replace the defaults. Starting from the
        // default (non-browser) set, enabling ChromeUse yields defaults + the
        // ChromeUse suite — the defaults survive.
        let defaults = vec!["Read".to_string(), "Bash".to_string(), "Edit".to_string()];
        let merged = toggle_browser_suite_names(
            defaults.clone(),
            BrowserSuite::ChromeUse.tool_names(),
            true,
        );
        // Every default survives.
        for name in &defaults {
            assert!(merged.contains(name), "default {name} lost: {merged:?}");
        }
        // Every ChromeUse tool is present.
        for name in BrowserSuite::ChromeUse.tool_names() {
            assert!(
                merged.iter().any(|n| n == name),
                "suite tool {name} missing: {merged:?}"
            );
        }
        assert_eq!(
            merged.len(),
            defaults.len() + BrowserSuite::ChromeUse.tool_names().len()
        );
    }

    #[test]
    fn toggle_browser_suite_enable_is_idempotent() {
        let once = toggle_browser_suite_names(
            vec!["Read".to_string()],
            BrowserSuite::WebExplore.tool_names(),
            true,
        );
        let twice =
            toggle_browser_suite_names(once.clone(), BrowserSuite::WebExplore.tool_names(), true);
        assert_eq!(once, twice, "re-enabling must not duplicate");
    }

    #[test]
    fn toggle_browser_suite_disable_strips_only_that_suite() {
        // Start with defaults + both suites; disabling ChromeUse removes only
        // the ChromeUse tools, leaving defaults + WebExplore intact.
        let mut names = vec!["Read".to_string(), "Bash".to_string()];
        names = toggle_browser_suite_names(names, BrowserSuite::ChromeUse.tool_names(), true);
        names = toggle_browser_suite_names(names, BrowserSuite::WebExplore.tool_names(), true);
        let stripped =
            toggle_browser_suite_names(names, BrowserSuite::ChromeUse.tool_names(), false);
        assert!(stripped.contains(&"Read".to_string()));
        assert!(stripped.contains(&"Bash".to_string()));
        for name in BrowserSuite::ChromeUse.tool_names() {
            assert!(!stripped.iter().any(|n| n == name), "{name} should be gone");
        }
        for name in BrowserSuite::WebExplore.tool_names() {
            assert!(stripped.iter().any(|n| n == name), "{name} should survive");
        }
    }

    #[test]
    fn project_browser_suites_requires_the_full_suite_active() {
        // A suite projects as active only when every one of its tool names is
        // selected; the browser-free default set projects nothing.
        let defaults = vec!["Read".to_string(), "Bash".to_string()];
        assert!(project_browser_suites(&defaults).is_empty());

        let partial: Vec<String> = BrowserSuite::ChromeUse.tool_names()[..3]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(project_browser_suites(&partial).is_empty());

        let mut full = defaults.clone();
        full.extend(
            BrowserSuite::ChromeUse
                .tool_names()
                .iter()
                .map(|s| s.to_string()),
        );
        assert_eq!(project_browser_suites(&full), vec![BrowserSuite::ChromeUse]);

        full.extend(
            BrowserSuite::WebExplore
                .tool_names()
                .iter()
                .map(|s| s.to_string()),
        );
        assert_eq!(
            project_browser_suites(&full),
            vec![BrowserSuite::ChromeUse, BrowserSuite::WebExplore]
        );
    }

    #[test]
    fn session_metadata_tags_host_thread_and_optional_team_parent() {
        let tagged = session_metadata("thread-1", Some("leader-1"));
        assert_eq!(tagged["host"], crate::host::current().slug());
        assert_eq!(tagged["thread"], "thread-1");
        assert_eq!(tagged["team"]["parent"], "leader-1");

        let plain = session_metadata("thread-1", None);
        assert_eq!(plain["host"], crate::host::current().slug());
        assert_eq!(plain["thread"], "thread-1");
        assert!(plain.get("team").is_none(), "no team key without a parent");
    }

    #[tokio::test]
    async fn permission_mode_sidecar_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-1.jsonl");

        // Fresh session: no sidecar -> default.
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            PermissionMode::WorkspaceWrite
        );

        write_approval_mode_sidecar(dir.path(), &session, PermissionMode::DangerFullAccess)
            .await
            .unwrap();
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            PermissionMode::DangerFullAccess
        );

        write_approval_mode_sidecar(dir.path(), &session, PermissionMode::ReadOnly)
            .await
            .unwrap();
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            PermissionMode::ReadOnly
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
        let mut history: Vec<HistoryEntry> = vec![
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
        ]
        .into_iter()
        .map(HistoryEntry::Message)
        .collect();
        attach_registry_displays(&mut history, &displays);

        let ui = |ix: usize| match &history[ix] {
            HistoryEntry::Message(m) => m.ui.as_ref().and_then(|ui| ui.display_text.clone()),
            _ => None,
        };
        let no_ui = |ix: usize| match &history[ix] {
            HistoryEntry::Message(m) => m.ui.is_none(),
            _ => false,
        };
        assert_eq!(ui(0).as_deref(), Some("/gitwork:deliver fast"));
        assert!(no_ui(1), "plain turn keeps no display text");
        assert!(no_ui(2), "tool result never consumes a display ordinal");
        assert!(no_ui(3), "assistant turns never get display text");
        assert_eq!(ui(4).as_deref(), Some("/healthz"));
    }

    #[tokio::test]
    async fn attach_user_attributions_restores_sidecar_authorship() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-author.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            user_attributions: [
                (
                    0usize,
                    pi_extensions::session_meta::UserAttributionMeta {
                        author: "lead".into(),
                        peer: false,
                        display_text: None,
                    },
                ),
                (
                    1usize,
                    pi_extensions::session_meta::UserAttributionMeta {
                        author: "Sailor".into(),
                        peer: true,
                        display_text: Some("unwrapped body".into()),
                    },
                ),
                // Beyond the transcript's ordinals: tolerated, attaches nowhere.
                (
                    5usize,
                    pi_extensions::session_meta::UserAttributionMeta {
                        author: "lead".into(),
                        peer: false,
                        display_text: None,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();

        let attributions = load_user_attributions(dir.path(), &session).await;
        // Same ordinal convention as the display forms: tool results and
        // assistant turns never consume one.
        let mut history: Vec<HistoryEntry> = vec![
            Message::user("plan seed".to_string()), // ordinal 0
            Message::assistant(vec![MessageContent::Text("ok".into())]),
            Message::user_with_content(vec![MessageContent::ToolResult(
                crate::language_model::LanguageModelToolResult {
                    tool_use_id: "tu_1".into(),
                    tool_name: "Read".into(),
                    is_error: false,
                    content: "ok".into(),
                },
            )]),
            Message::user("[from Sailor]: wrapped".to_string()), // ordinal 1
        ]
        .into_iter()
        .map(HistoryEntry::Message)
        .collect();
        attach_user_attributions(&mut history, &attributions);

        let ui_at = |ix: usize| match &history[ix] {
            HistoryEntry::Message(m) => m.ui.clone(),
            _ => None,
        };
        let seed = ui_at(0).expect("seed carries attribution");
        assert_eq!(seed.author, Some(crate::message::MessageAuthor::Lead));
        assert!(!seed.peer);
        assert!(ui_at(1).is_none(), "assistant turns stay unattributed");
        let peer = ui_at(3).expect("peer delivery carries attribution");
        assert_eq!(
            peer.author,
            Some(crate::message::MessageAuthor::Agent("Sailor".into()))
        );
        assert!(peer.peer);
        assert_eq!(
            peer.display_text.as_deref(),
            Some("unwrapped body"),
            "the send-time display form survives the sidecar round-trip"
        );
        assert!(
            ui_at(2).is_none(),
            "tool results never consume an ordinal nor carry attribution"
        );
    }

    #[tokio::test]
    async fn clear_user_chrome_drops_sidecar_ordinals() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-display-clear.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            registry_displays: [(0usize, "/gitwork:deliver fast".to_string())]
                .into_iter()
                .collect(),
            user_attributions: [(
                0usize,
                pi_extensions::session_meta::UserAttributionMeta {
                    author: "lead".into(),
                    peer: false,
                    display_text: None,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();

        clear_user_chrome(dir.path(), &session).await;

        let displays = load_registry_displays(dir.path(), &session).await;
        assert!(
            displays.is_empty(),
            "a compaction clears the stale display ordinals"
        );
        let attributions = load_user_attributions(dir.path(), &session).await;
        assert!(
            attributions.is_empty(),
            "a compaction clears the stale attributions"
        );
    }

    #[tokio::test]
    async fn clear_user_chrome_is_noop_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-display-empty.jsonl");

        clear_user_chrome(dir.path(), &session).await;

        let displays = load_registry_displays(dir.path(), &session).await;
        assert!(displays.is_empty());
    }

    #[tokio::test]
    async fn permission_mode_sidecar_tolerates_unknown_values() {
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
            PermissionMode::WorkspaceWrite,
            "unknown persisted modes fall back to the bounded default"
        );
    }

    #[tokio::test]
    async fn permission_mode_write_preserves_other_sidecar_fields() {
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

        write_approval_mode_sidecar(dir.path(), &session, PermissionMode::DangerFullAccess)
            .await
            .unwrap();

        let loaded = pi_extensions::session_meta::load(dir.path(), &session)
            .await
            .unwrap();
        assert_eq!(loaded.title.as_deref(), Some("my thread"));
        assert_eq!(loaded.project.as_deref(), Some("/tmp/proj"));
        assert_eq!(loaded.approval_mode.as_deref(), Some("danger-full-access"));
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
    fn agent_child_text_delta_maps_to_subagent_child() {
        let events =
            adapt::agent_event_to_thread_events(&pi::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                arguments: serde_json::json!({}),
                partial_result: serde_json::json!({
                    "subagent_event": { "kind": "text", "text": "found it" }
                }),
            });
        assert_eq!(events.len(), 1);
        match &events[0] {
            crate::thread::ThreadEvent::SubagentChild { id, child } => {
                assert_eq!(id, "call-1");
                assert_eq!(
                    child,
                    &crate::thread::SubagentChildEvent::Text("found it".into())
                );
            }
            other => panic!("expected SubagentChild, got {other:?}"),
        }
    }

    #[test]
    fn agent_child_tool_lifecycle_maps_to_child_and_rail_activity() {
        let start = adapt::agent_event_to_thread_events(
            &pi::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                arguments: serde_json::json!({}),
                partial_result: serde_json::json!({
                    "subagent_event": { "kind": "tool_start", "id": "child-1", "tool": "Read", "summary_key": "path", "summary": "src/main.rs" }
                }),
            },
        );
        assert_eq!(start.len(), 2, "drill-down event + rail activity");
        assert!(matches!(
            &start[0],
            crate::thread::ThreadEvent::SubagentChild {
                child: crate::thread::SubagentChildEvent::ToolStart { .. },
                ..
            }
        ));
        match &start[1] {
            crate::thread::ThreadEvent::SubagentProgress {
                latest_activity, ..
            } => assert_eq!(latest_activity.as_deref(), Some("▸ Read src/main.rs")),
            other => panic!("expected SubagentProgress, got {other:?}"),
        }

        let end = adapt::agent_event_to_thread_events(
            &pi::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                arguments: serde_json::json!({}),
                partial_result: serde_json::json!({
                    "subagent_event": { "kind": "tool_end", "id": "child-1", "tool": "Read", "is_error": true }
                }),
            },
        );
        assert_eq!(end.len(), 2);
        assert!(matches!(
            &end[0],
            crate::thread::ThreadEvent::SubagentChild {
                child: crate::thread::SubagentChildEvent::ToolEnd { is_error: true, .. },
                ..
            }
        ));
    }

    #[test]
    fn bash_output_update_still_maps_to_tool_output() {
        let events =
            adapt::agent_event_to_thread_events(&pi::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-3".into(),
                tool_name: "Bash".into(),
                arguments: serde_json::json!({}),
                partial_result: serde_json::json!({ "output": "line one" }),
            });
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            crate::thread::ThreadEvent::ToolOutput { chunk, .. } if chunk == "line one"
        ));
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

    fn test_engine_state() -> Arc<EngineState> {
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel();
        let model_slot = Arc::new(Mutex::new(None));
        let gate = Arc::new(ApprovalGate::new(notice_tx, Arc::clone(&model_slot)));
        Arc::new(EngineState {
            running: AtomicBool::new(false),
            session_start_fired: AtomicBool::new(false),
            history: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
            notes_gen: AtomicU64::new(0),
            pending_ui_notes: Mutex::new(Vec::new()),
            request_usage: Mutex::new(HashMap::new()),
            per_model_last_usage: Mutex::new(HashMap::new()),
            cumulative: Mutex::new(TokenUsage::default()),
            per_model: Mutex::new(HashMap::new()),
            cumulative_cost: Mutex::new(0.0),
            per_model_cost: Mutex::new(HashMap::new()),
            model: model_slot,
            sessions: Mutex::new(Vec::new()),
            active_path: Mutex::new(None),
            pending_browser_suite: Mutex::new(None),
            pending_session_cmds: Mutex::new(Vec::new()),
            gate,
            plan: crate::plan_mode::PlanSessionState::new(),
            goal_bridge: None,
            goal_continuation_reserved: AtomicBool::new(false),
            goal_continuation_round: Mutex::new(None),
            worktree: crate::worktree::new_state(),
        })
    }

    fn partial_assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: None,
            raw_stop_reason: None,
            usage: Box::new(pi::types::Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    /// The live-history mirror serves completed messages plus the streaming
    /// partial (kernel `streaming_message` parity), and reports change only
    /// when the snapshot actually moved (the idle-tick guard).
    #[test]
    fn sync_live_history_mirrors_completed_plus_streaming_and_reports_change() {
        let state = test_engine_state();
        let live = Arc::new(Mutex::new(LiveTranscript::default()));
        live.lock().unwrap().messages.push(AgentMessage::user("hi"));
        live.lock().unwrap().streaming = Some(partial_assistant("part"));

        assert!(sync_live_history(&live, &state));
        {
            let history = state.history.lock().unwrap();
            assert_eq!(history.len(), 2);
            assert!(matches!(
                &history[0],
                HistoryEntry::Message(m)
                    if matches!(&m.content[0], crate::language_model::MessageContent::Text(t) if t == "hi")
            ));
            assert!(matches!(
                &history[1],
                HistoryEntry::Message(m)
                    if matches!(&m.content[0], crate::language_model::MessageContent::Text(t) if t == "part")
            ));
        }

        // Identical snapshot: no change (a run parked on an approval verdict
        // streams nothing, so the facade notice is skipped).
        assert!(!sync_live_history(&live, &state));

        // The streaming partial growing re-syncs.
        live.lock().unwrap().streaming = Some(partial_assistant("partial-answer"));
        assert!(sync_live_history(&live, &state));
        let history = state.history.lock().unwrap();
        assert!(matches!(
            &history[1],
            HistoryEntry::Message(m)
                if matches!(&m.content[0], crate::language_model::MessageContent::Text(t) if t == "partial-answer")
        ));
    }

    /// A stream that issues one tool call then stops, recording the model of
    /// every provider request and parking on barriers so the test can
    /// interleave a mid-run model switch between the two turns.
    struct MidRunModelStream {
        calls: std::sync::atomic::AtomicUsize,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        turn1_started: Arc<tokio::sync::Notify>,
        release1: Arc<tokio::sync::Notify>,
        turn2_started: Arc<tokio::sync::Notify>,
        release2: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl pi::agent_loop::StreamFn for MidRunModelStream {
        async fn stream(
            &self,
            context: &pi::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<pi::types::AgentEvent>,
        ) -> Result<pi::types::AgentMessage, anyhow::Error> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.seen.lock().unwrap().push(context.model.id.clone());
            if n == 0 {
                self.turn1_started.notify_waiters();
                self.release1.notified().await;
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"message": "hi"}),
                        thought_signature: None,
                    }],
                    model: context.model.id.clone(),
                    provider: context.model.provider.clone(),
                    api: context.model.api.clone(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(pi::types::StopReason::ToolUse),
                    usage: Box::new(pi::types::Usage {
                        input_tokens: 100,
                        output_tokens: 10,
                        ..Default::default()
                    }),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            } else {
                self.turn2_started.notify_waiters();
                self.release2.notified().await;
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                        signature: None,
                    }],
                    model: context.model.id.clone(),
                    provider: context.model.provider.clone(),
                    api: context.model.api.clone(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(pi::types::StopReason::Stop),
                    usage: Box::new(pi::types::Usage {
                        input_tokens: 100,
                        output_tokens: 10,
                        ..Default::default()
                    }),
                    error_message: None,
                    timestamp: chrono::Utc::now(),
                })
            }
        }
    }

    /// The `echo` tool the mid-run stream calls, so the run spans two turns.
    struct EchoTool;

    #[async_trait::async_trait]
    impl pi::tool::AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes the input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!(
                {"type": "object", "properties": {"message": {"type": "string"}}}
            )
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Value,
            _signal: tokio_util::sync::CancellationToken,
            _ctx: &dyn pi::tool::ToolContext,
        ) -> Result<pi::tool::AgentToolResult, pi::tool::ToolError> {
            Ok(pi::tool::AgentToolResult::text(
                params["message"].as_str().unwrap_or("no message"),
            ))
        }
    }

    fn test_model_switched() -> PiModel {
        PiModel {
            provider: "test".into(),
            api: "test".into(),
            id: "new".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: pi::types::ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    fn test_model() -> PiModel {
        PiModel {
            provider: "test".into(),
            api: "test".into(),
            id: "test".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: pi::types::ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    /// A model switch arriving while a turn is in flight must reach the next
    /// provider request: `drive_run` routes `SetModel` through the harness
    /// handle (the turn runtime) instead of dropping it, so the turn after
    /// the switch streams under the new model and the session attributes its
    /// usage to it. Regression for the mid-conversation switch that showed
    /// the new model in the UI while requests still ran the old one.
    #[tokio::test]
    async fn mid_run_model_switch_applies_to_next_turn_and_stats() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(MidRunModelStream {
            calls: std::sync::atomic::AtomicUsize::new(0),
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            turn1_started: Arc::new(tokio::sync::Notify::new()),
            release1: Arc::new(tokio::sync::Notify::new()),
            turn2_started: Arc::new(tokio::sync::Notify::new()),
            release2: Arc::new(tokio::sync::Notify::new()),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: pi::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn pi::agent_loop::StreamFn>)
        });
        let runtime = ModelRuntime::new(resolver);

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(runtime)
            .with_model(test_model())
            .with_tools(vec![Arc::new(EchoTool) as Arc<dyn pi::tool::AgentTool>])
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let state = test_engine_state();
        let live = Arc::new(Mutex::new(LiveTranscript::default()));
        let mut run_steers = Vec::new();
        let mut shutdown_after_run = false;
        let mut pi_model = test_model();

        let handle = session.handle();
        let sessions_path = dir.path().join("sessions");
        let active_session_path = session.path().clone();
        let run = drive_run(
            session.prompt("first turn"),
            &handle,
            &mut cmd_rx,
            &mut run_steers,
            &mut shutdown_after_run,
            live,
            &state,
            &notice_tx,
            &mut pi_model,
            &sessions_path,
            &active_session_path,
        );
        let new_model = test_model_switched();

        let ((result, _aborted), ()) = tokio::join!(run, async {
            // Turn 1 in flight; switch the model before it resumes. While
            // the run parks on the barrier, drive_run's select polls the
            // command channel, so the switch lands before turn 1 returns.
            stream.turn1_started.notified().await;
            cmd_tx
                .send(SessionCmd::SetModel(new_model.clone()))
                .unwrap();
            for _ in 0..10_000 {
                if state.model.lock().unwrap().as_ref().map(|m| m.id.as_str()) == Some("new") {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(
                state.model.lock().unwrap().as_ref().map(|m| m.id.as_str()),
                Some("new"),
                "drive_run must apply the mid-run SetModel while the turn is in flight"
            );
            stream.release1.notify_waiters();
            // Turn 2 streams under the switched model; release it.
            stream.turn2_started.notified().await;
            stream.release2.notify_waiters();
        });
        result.unwrap();

        assert_eq!(
            *stream.seen.lock().unwrap(),
            vec!["test".to_string(), "new".to_string()],
            "the turn after the mid-run switch must stream under the new model"
        );
        assert_eq!(
            pi_model.id, "new",
            "the actor's working model follows the switch"
        );
        // The session attributes the switched turn's usage to the new model:
        // the per-model breakdown now carries both identities.
        let stats = session.session_stats().await.unwrap();
        assert!(
            stats.per_model.iter().any(|e| e.key == "test/new"),
            "switched-turn usage must enter the per-model stats: {:?}",
            stats.per_model
        );
        // The switch persisted as a model_change entry, so a reload
        // attributes the same history the same way.
        let jsonl = tokio::fs::read_to_string(session.path()).await.unwrap();
        assert!(
            jsonl.contains("\"type\":\"model_change\"") && jsonl.contains("\"modelId\":\"new\""),
            "the mid-run switch must persist a model_change entry for the new model"
        );
    }

    /// `AppendUiNote` arriving while a turn is in flight must not be dropped
    /// like the other unserviceable mid-run commands: the card merges into
    /// the mirror immediately (a mid-run switch-back renders it) and parks
    /// its persistence for the idle loop, which appends the `custom` entry
    /// at the leaf once the run owns no borrow of the session.
    #[tokio::test]
    async fn mid_run_append_ui_note_mirrors_now_and_parks_persist() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(MidRunModelStream {
            calls: std::sync::atomic::AtomicUsize::new(0),
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            turn1_started: Arc::new(tokio::sync::Notify::new()),
            release1: Arc::new(tokio::sync::Notify::new()),
            turn2_started: Arc::new(tokio::sync::Notify::new()),
            release2: Arc::new(tokio::sync::Notify::new()),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: pi::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn pi::agent_loop::StreamFn>)
        });
        let runtime = ModelRuntime::new(resolver);

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(runtime)
            .with_model(test_model())
            .with_tools(vec![Arc::new(EchoTool) as Arc<dyn pi::tool::AgentTool>])
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let state = test_engine_state();
        let live = Arc::new(Mutex::new(LiveTranscript::default()));
        let mut run_steers = Vec::new();
        let mut shutdown_after_run = false;
        let mut pi_model = test_model();

        let handle = session.handle();
        let sessions_path = dir.path().join("sessions");
        let active_session_path = session.path().clone();
        let run = drive_run(
            session.prompt("first turn"),
            &handle,
            &mut cmd_rx,
            &mut run_steers,
            &mut shutdown_after_run,
            live,
            &state,
            &notice_tx,
            &mut pi_model,
            &sessions_path,
            &active_session_path,
        );
        let record = UiNoteRecord {
            kind: crate::db::UiNoteKind::Notice,
            data: serde_json::json!({ "text": "mid-run card" }),
        };

        let ((result, _aborted), ()) = tokio::join!(run, async {
            stream.turn1_started.notified().await;
            cmd_tx.send(SessionCmd::AppendUiNote(record)).unwrap();
            // The mirror takes the card while the turn is still in flight.
            for _ in 0..10_000 {
                if state.notes.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(
                state.notes.lock().unwrap().len(),
                1,
                "mid-run AppendUiNote must merge into the mirror immediately"
            );
            assert!(
                matches!(
                    state.history.lock().unwrap().last(),
                    Some(HistoryEntry::Note(_))
                ),
                "the mirrored tail is the note"
            );
            stream.release1.notify_waiters();
            stream.turn2_started.notified().await;
            stream.release2.notify_waiters();
        });
        // Settlement persists the parked note BEFORE the authoritative
        // sync, so the rebuilt mirror retains it — no loss window between
        // the mid-run mirror and the next reload.
        let repo = pi::session::repository::SessionRepository::new(&sessions_path);
        settle_run(
            &result,
            false,
            &session,
            &state,
            &repo,
            &sessions_path,
            &cwd,
            &notice_tx,
            &mut run_steers,
        )
        .await;
        result.unwrap();

        assert!(
            state.pending_ui_notes.lock().unwrap().is_empty(),
            "settlement drains the parked queue"
        );
        assert!(
            matches!(
                state.history.lock().unwrap().last(),
                Some(HistoryEntry::Note(_))
            ),
            "the authoritative rebuild must retain the parked note"
        );
        assert_eq!(
            state.notes.lock().unwrap().len(),
            1,
            "the positioned-note mirror survives settlement"
        );
        let jsonl = tokio::fs::read_to_string(session.path()).await.unwrap();
        assert!(
            jsonl.contains("\"customType\":\"manox_ui_note\"") && jsonl.contains("mid-run card"),
            "settlement must persist the parked note"
        );
    }

    /// A stream that answers every provider request immediately with a
    /// terminal assistant message carrying the request's model identity.
    struct StaticStream;

    #[async_trait::async_trait]
    impl pi::agent_loop::StreamFn for StaticStream {
        async fn stream(
            &self,
            context: &pi::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<pi::types::AgentEvent>,
        ) -> Result<pi::types::AgentMessage, anyhow::Error> {
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                    signature: None,
                }],
                model: context.model.id.clone(),
                provider: context.model.provider.clone(),
                api: context.model.api.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(pi::types::StopReason::Stop),
                usage: Box::new(pi::types::Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    /// Restores the two test models from their session references.
    struct TestModelCatalog;

    impl pi::coding_agent::model_runtime::ModelCatalog for TestModelCatalog {
        fn resolve(&self, provider: &str, model_id: &str) -> Option<PiModel> {
            match (provider, model_id) {
                ("test", "test") => Some(test_model()),
                ("test", "new") => Some(test_model_switched()),
                _ => None,
            }
        }
    }

    /// A reopened session must project its own persisted model onto the
    /// harness: the restore path builds with no model override so
    /// `builder.open()` restores the session model, and
    /// `adopt_session_model` mirrors it into the actor's working model and
    /// the shared slot. Regression for the reopen that showed the default
    /// model in the composer selector.
    #[tokio::test]
    async fn reopened_session_restores_its_own_model() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let sessions = dir.path().join("sessions");
        let agent = dir.path().join("agent");
        tokio::fs::create_dir_all(&sessions).await.unwrap();
        tokio::fs::create_dir_all(&agent).await.unwrap();

        let resolver: pi::agent_loop::StreamResolver =
            Arc::new(
                |_m: &PiModel| Ok(Arc::new(StaticStream) as Arc<dyn pi::agent_loop::StreamFn>),
            );
        let runtime = ModelRuntime::new(resolver).with_catalog(Arc::new(TestModelCatalog));

        // Phase 1: a session that ran under `test` and switched to `new`.
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&sessions)
            .with_agent_dir(&agent)
            .with_model_runtime(runtime.clone())
            .with_model(test_model())
            .with_tools(vec![Arc::new(EchoTool) as Arc<dyn pi::tool::AgentTool>])
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();
        let path = session.path().clone();
        // A prompt materializes the JSONL (the deferred-first contract
        // writes disk only once an assistant message exists).
        let _ = session.prompt("first turn").await.unwrap();
        session.set_model(test_model_switched()).await.unwrap();
        let jsonl = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            jsonl.contains("\"type\":\"model_change\"") && jsonl.contains("\"modelId\":\"new\""),
            "the switch must persist a model_change entry for the new model"
        );
        session.close().await.unwrap();

        // Phase 2: the fixed restore path — no model override on the
        // builder, so `open()` restores the session's own model.
        let reopened = create_agent_session()
            .with_agent_dir(&agent)
            .with_model_runtime(runtime)
            .with_tools(vec![Arc::new(EchoTool) as Arc<dyn pi::tool::AgentTool>])
            .with_system_prompt("You are a test assistant.")
            .open(path)
            .await
            .unwrap();
        assert_eq!(
            reopened.model().id,
            "new",
            "the reopened session must restore its own model, not the default"
        );

        let state = test_engine_state();
        let mut pi_model = test_model();
        adopt_session_model(&reopened, &mut pi_model, &state);
        assert_eq!(
            pi_model.id, "new",
            "the actor's working model follows the restored session"
        );
        assert_eq!(
            state.model.lock().unwrap().as_ref().map(|m| m.id.as_str()),
            Some("new"),
            "the shared model slot follows the restored session"
        );
    }

    #[test]
    fn worktree_policy_scopes_to_the_active_worktree_and_falls_back() {
        let base = crate::sandbox::canonicalize_best_effort(&std::env::temp_dir())
            .join("manox-wt-policy-proj");
        let wt = base.join("wt");
        let git = base.join(".git");
        let state = crate::worktree::new_state();
        // No active worktree → plain project policy, no granted extras: the
        // worktree/common-dir roots are absent from the seatbelt profile.
        let p = worktree_policy(&wt, &state);
        let sb_none = p.render_seatbelt(crate::thread::PermissionMode::WorkspaceWrite);
        assert!(
            !sb_none.contains(&format!(
                "(allow file-write* (subpath \"{}\"))",
                crate::sandbox::canonicalize_best_effort(&git).display()
            )),
            "no worktree roots without a binding: {sb_none}"
        );
        // Active worktree matching the session cwd → additive roots: the
        // workspace-write profile admits the worktree, its git common dir,
        // and the pre-enter project root.
        *state.lock().unwrap() = Some(pi_extensions::session_meta::WorktreeMeta {
            worktree_path: wt.display().to_string(),
            branch: "b".into(),
            original_session_path: "x".into(),
            original_cwd: base.display().to_string(),
            git_common_dir: git.display().to_string(),
        });
        let sb = worktree_policy(&wt, &state)
            .render_seatbelt(crate::thread::PermissionMode::WorkspaceWrite);
        for root in [&wt, &base, &git] {
            assert!(
                sb.contains(&format!(
                    "(allow file-write* (subpath \"{}\"))",
                    crate::sandbox::canonicalize_best_effort(root).display()
                )),
                "worktree root admitted: {sb}"
            );
        }
        // Session cwd back at the original repo (Exit) → no worktree extras
        // again, not the stale granted roots.
        let sb_back = worktree_policy(&base, &state)
            .render_seatbelt(crate::thread::PermissionMode::WorkspaceWrite);
        assert!(
            !sb_back.contains(&format!(
                "(allow file-write* (subpath \"{}\"))",
                crate::sandbox::canonicalize_best_effort(&git).display()
            )),
            "no stale worktree roots after exit: {sb_back}"
        );
    }

    #[tokio::test]
    async fn reasoning_effort_sidecar_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-effort.jsonl");

        // Fresh session: no sidecar -> default High.
        assert_eq!(
            load_reasoning_effort(dir.path(), &session).await,
            ReasoningEffort::High
        );

        write_reasoning_effort_sidecar(dir.path(), &session, ReasoningEffort::Max)
            .await
            .unwrap();
        assert_eq!(
            load_reasoning_effort(dir.path(), &session).await,
            ReasoningEffort::Max
        );

        write_reasoning_effort_sidecar(dir.path(), &session, ReasoningEffort::High)
            .await
            .unwrap();
        assert_eq!(
            load_reasoning_effort(dir.path(), &session).await,
            ReasoningEffort::High
        );
    }

    #[tokio::test]
    async fn reasoning_effort_sidecar_tolerates_unknown_values() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-effort-unknown.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();
        assert_eq!(
            load_reasoning_effort(dir.path(), &session).await,
            ReasoningEffort::High,
            "unknown persisted efforts fall back to the default"
        );
    }

    fn assistant_usage(cache_read: u64) -> pi::types::Usage {
        pi::types::Usage {
            input_tokens: 10,
            cache_read_input_tokens: cache_read,
            ..Default::default()
        }
    }

    fn assistant_request(usage: pi::types::Usage) -> AgentMessage {
        AgentMessage::Assistant {
            content: Vec::new(),
            model: "deepseek-v4-flash".into(),
            provider: "DeepSeek".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(pi::types::StopReason::Stop),
            raw_stop_reason: None,
            usage: Box::new(usage),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn request_attribution_keys_turns_and_keeps_only_latest_request_per_model() {
        let (u1, u2, u3) = (
            assistant_usage(100),
            assistant_usage(200),
            assistant_usage(300),
        );
        let messages = vec![
            AgentMessage::User {
                content: Vec::new(),
                timestamp: chrono::Utc::now(),
            },
            assistant_request(u1.clone()),
            assistant_request(u2.clone()),
            AgentMessage::User {
                content: Vec::new(),
                timestamp: chrono::Utc::now(),
            },
            assistant_request(u3.clone()),
        ];
        let history: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&messages)
            .into_iter()
            .map(HistoryEntry::Message)
            .collect();
        let (per_turn, per_model_last) = request_attribution(&history, &messages);
        let id = |ix: usize| match &history[ix] {
            HistoryEntry::Message(m) => m.id.clone(),
            _ => String::new(),
        };

        // Each turn accumulates under its own triggering user message.
        assert_eq!(per_turn.len(), 2);
        assert_eq!(per_turn[&id(0)], to_token_usage(&u1) + to_token_usage(&u2));
        assert_eq!(per_turn[&id(3)], to_token_usage(&u3));

        // The budget numerator is the single latest request, never a sum.
        let last = per_model_last
            .get("DeepSeek/deepseek-v4-flash")
            .expect("latest request keyed by provider/model");
        assert_eq!(last.cache_read_input_tokens, 300);
    }

    #[test]
    #[should_panic(expected = "out of lockstep")]
    fn request_attribution_backstop_trips_on_leftover_mapped_rows() {
        let messages = vec![
            AgentMessage::User {
                content: Vec::new(),
                timestamp: chrono::Utc::now(),
            },
            assistant_request(assistant_usage(100)),
        ];
        let mut history: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&messages)
            .into_iter()
            .map(HistoryEntry::Message)
            .collect();
        // Simulate adapt drift: one mapped row the walk never consumes.
        history.push(history[0].clone());
        let _ = request_attribution(&history, &messages);
    }

    #[test]
    #[should_panic(expected = "out of lockstep")]
    fn request_attribution_backstop_trips_on_over_consumption() {
        let messages = vec![
            AgentMessage::User {
                content: Vec::new(),
                timestamp: chrono::Utc::now(),
            },
            assistant_request(assistant_usage(100)),
        ];
        // Simulate adapt drift in the other direction: the walk consumes an
        // id for a message the mapping would not emit.
        let history: Vec<HistoryEntry> = adapt::harness_messages_to_messages(&messages[..1])
            .into_iter()
            .map(HistoryEntry::Message)
            .collect();
        let _ = request_attribution(&history, &messages);
    }

    #[test]
    fn agent_tool_description_lists_built_in_subagents_with_capability() {
        let mut registry = pi::ext_point_agent::AgentRegistry::new();
        pi_extensions::agents::register_defaults(&mut registry);
        let subagents: Vec<crate::prompt::SubagentTypeData> = registry
            .all()
            .iter()
            .map(|def| crate::prompt::SubagentTypeData {
                name: def.name.clone(),
                capability: subagent_capability(def),
                description: def.description.clone(),
            })
            .collect();
        let rendered = crate::prompt::render(
            crate::prompt::PromptTemplate::AgentToolDescription,
            crate::language::Language::En,
            &crate::prompt::AgentToolDescriptionData { subagents },
        )
        .expect("AgentToolDescription renders");
        assert!(
            rendered.contains("Explore (read-only)"),
            "Explore read-only tag present: {rendered}"
        );
        assert!(
            rendered.contains("Sailor (write+bash)"),
            "Sailor write+bash tag present: {rendered}"
        );
        assert!(
            rendered.contains("synchronously"),
            "template declares synchronous read-only subagents: {rendered}"
        );
        assert!(
            rendered.contains("asynchronously"),
            "template declares asynchronous write+bash subagents: {rendered}"
        );
    }

    /// The capability tag distinguishes write+bash defs (Sailor) from
    /// read-only defs (Explore). It is the routing key for the plan-mode
    /// read-only resolver, which blocks write/bash subagents from being
    /// dispatched while plan mode is active.
    #[test]
    fn subagent_capability_tags_write_bash_vs_read_only() {
        let mut registry = pi::ext_point_agent::AgentRegistry::new();
        pi_extensions::agents::register_defaults(&mut registry);
        let sailor = registry.get("Sailor").expect("Sailor registered");
        let explore = registry.get("Explore").expect("Explore registered");
        assert_eq!(subagent_capability(sailor), "write+bash");
        assert_ne!(
            subagent_capability(sailor),
            "read-only",
            "Sailor routes async (not read-only)"
        );
        assert_eq!(
            subagent_capability(explore),
            "read-only",
            "Explore stays synchronous"
        );
    }

    #[test]
    fn subagent_bash_description_forbids_background_and_drops_host_claims() {
        // The subagent Bash wrapper rejects `run_in_background` (N1: no
        // bare-spawn escape hatch) and its description must not carry the
        // host-session BashTool's false claims (state persists / approval).
        assert!(
            SUBAGENT_BASH_DESCRIPTION.contains("run_in_background"),
            "description flags background refusal"
        );
        assert!(
            !SUBAGENT_BASH_DESCRIPTION.contains("State persists"),
            "no state-persistence claim"
        );
        assert!(
            !SUBAGENT_BASH_DESCRIPTION.contains("requires user approval"),
            "no approval-gating claim for the ungated subagent session"
        );
    }

    /// `SubagentBashTool` must reject `run_in_background` (N1: no bare-spawn
    /// escape hatch) while leaving foreground calls untouched. Uses a marker
    /// inner so the gate is exercised without constructing a real BashTool.
    struct MarkerBash;
    #[async_trait::async_trait]
    impl pi::tool::AgentTool for MarkerBash {
        fn name(&self) -> &str {
            "Bash"
        }
        fn description(&self) -> &str {
            "marker"
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn requires_approval(&self, _: &serde_json::Value) -> bool {
            false
        }
        fn parameters_schema(&self) -> serde_json::Value {
            // Mirror the kernel BashTool's shape so the wrapper's strip is
            // exercised (run_in_background + unsandboxed get removed).
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "run_in_background": {"type": "boolean"},
                    "unsandboxed": {"type": "boolean"}
                },
                "required": ["command"]
            })
        }
        async fn execute(
            &self,
            _: &str,
            _: serde_json::Value,
            _: tokio_util::sync::CancellationToken,
            _: &dyn pi::tool::ToolContext,
        ) -> Result<pi::tool::AgentToolResult, pi::tool::ToolError> {
            Ok(pi::tool::AgentToolResult::text("foreground-ok"))
        }
    }

    #[tokio::test]
    async fn subagent_bash_refuses_background_but_allows_foreground() {
        use std::path::PathBuf;
        let tool = SubagentBashTool {
            inner: Arc::new(MarkerBash),
        };
        let ctx = pi::tool::LocalToolContext::new(
            Arc::new(pi::env::TokioExecutionEnv::new(PathBuf::from("/tmp"))),
            PathBuf::from("/tmp"),
            Arc::new(pi::tool::ToolState::new()),
        );
        // Background: refused at the gate; the inner is never reached.
        let bg = tool
            .execute(
                "c1",
                serde_json::json!({"command": "echo hi", "run_in_background": true}),
                tokio_util::sync::CancellationToken::new(),
                &ctx,
            )
            .await;
        let err = bg.unwrap_err().to_string();
        assert!(
            err.contains("not available inside a subagent"),
            "background refused: {err}"
        );
        // Foreground: the gate passes and the inner runs.
        let fg = tool
            .execute(
                "c2",
                serde_json::json!({"command": "echo hi"}),
                tokio_util::sync::CancellationToken::new(),
                &ctx,
            )
            .await
            .expect("foreground gate passes");
        let fg_text: String = fg
            .content
            .iter()
            .filter_map(|b| {
                if let pi::types::ContentBlock::Text { text, .. } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(fg_text, "foreground-ok", "foreground reached the inner");
    }

    #[test]
    fn subagent_bash_schema_strips_background_and_escalation_fields() {
        let tool = SubagentBashTool {
            inner: Arc::new(MarkerBash),
        };
        let schema = tool.parameters_schema();
        let props = schema["properties"]
            .as_object()
            .expect("schema has properties");
        assert!(
            !props.contains_key("run_in_background"),
            "run_in_background stripped from the subagent schema"
        );
        assert!(
            !props.contains_key("sandbox_permissions"),
            "sandbox_permissions stripped from the subagent schema"
        );
        assert!(
            !props.contains_key("justification"),
            "justification stripped from the subagent schema"
        );
        assert!(props.contains_key("command"), "command still advertised");
    }

    /// A dispatched subagent session persists under the host-injected session
    /// directory with its dispatch lineage in the header metadata; without an
    /// injected directory the transcript stays in a tempdir whose guard the
    /// caller must hold (examples/tests lifecycle).
    #[tokio::test]
    async fn subagent_session_persists_under_host_dir_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let resolver: pi::agent_loop::StreamResolver =
            Arc::new(
                |_m: &PiModel| Ok(Arc::new(StaticStream) as Arc<dyn pi::agent_loop::StreamFn>),
            );
        let runtime = ModelRuntime::new(resolver);

        let mut registry = AgentRegistry::new();
        register_defaults(&mut registry);
        let registry = Arc::new(registry);
        let tools: Vec<Arc<dyn pi::tool::AgentTool>> = vec![
            Arc::new(pi::tools::read::ReadTool),
            Arc::new(pi::tools::grep::GrepTool),
            Arc::new(pi::tools::glob::GlobTool),
            Arc::new(pi::tools::ls::LsTool),
        ];
        let ctx = pi::tool::LocalToolContext::new(
            Arc::new(pi::env::TokioExecutionEnv::new(cwd.clone())),
            cwd.clone(),
            Arc::new(pi::tool::ToolState::new()),
        );

        // Host-injected directory: the transcript persists there and the
        // header carries the dispatch lineage.
        let subagents = dir.path().join("subagents");
        let tool = SubagentTool::new(Arc::clone(&registry), tools.clone())
            .with_model_runtime(runtime.clone())
            .with_model(test_model())
            .with_session_dir(subagents.clone());
        let (mut session, guard, worktree) = tool
            .spawn_subagent_session("Explore", None, &ctx, vec![], Some("thread-parent"))
            .await
            .unwrap();
        assert!(guard.is_none(), "persistent dir spawns no tempdir guard");
        assert!(worktree.is_none());
        let _ = session.prompt("hi").await.unwrap();
        let path = session.path().clone();
        assert_eq!(path.parent().unwrap(), subagents);
        let header = tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(
            header.contains("\"metadata\":{\"subagent\":{")
                && header.contains("\"type\":\"Explore\"")
                && header.contains("\"parent\":\"thread-parent\""),
            "header must carry the subagent lineage: {header}"
        );

        // No injected directory: the throwaway tempdir lifecycle stays, and
        // the guard is the caller's only handle to the transcript.
        let tool = SubagentTool::new(Arc::clone(&registry), tools)
            .with_model_runtime(runtime)
            .with_model(test_model());
        let (_session, guard, _worktree) = tool
            .spawn_subagent_session("Explore", None, &ctx, vec![], None)
            .await
            .unwrap();
        assert!(guard.is_some(), "uninjected spawn keeps the tempdir guard");
    }

    fn worktree_fixture() -> pi_extensions::session_meta::WorktreeMeta {
        pi_extensions::session_meta::WorktreeMeta {
            worktree_path: "/tmp/wt".into(),
            branch: "b".into(),
            original_session_path: "/sessions/source.jsonl".into(),
            original_cwd: "/proj/a".into(),
            git_common_dir: "/proj/a/.git".into(),
        }
    }

    #[tokio::test]
    async fn fork_sidecar_inherit_seeds_source_identity() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        let fork = dir.path().join("fork.jsonl");
        pi_extensions::session_meta::update(dir.path(), &source, |m| {
            m.title = Some("the title".into());
            m.project = Some("/proj/a".into());
        })
        .await
        .unwrap();
        fork_sidecar_inherit(dir.path(), &source, &fork, worktree_fixture())
            .await
            .unwrap();
        let meta = pi_extensions::session_meta::load(dir.path(), &fork)
            .await
            .unwrap();
        assert_eq!(meta.title.as_deref(), Some("the title"));
        assert_eq!(meta.project.as_deref(), Some("/proj/a"));
        assert_eq!(meta.worktree, Some(worktree_fixture()));
    }

    #[tokio::test]
    async fn fork_sidecar_inherit_degrades_without_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("missing.jsonl");
        let fork = dir.path().join("fork.jsonl");
        // No source sidecar on disk: the binding lands alone, no error.
        fork_sidecar_inherit(dir.path(), &source, &fork, worktree_fixture())
            .await
            .unwrap();
        let meta = pi_extensions::session_meta::load(dir.path(), &fork)
            .await
            .unwrap();
        assert!(meta.title.is_none());
        assert!(meta.project.is_none());
        assert_eq!(meta.worktree, Some(worktree_fixture()));
    }
}
