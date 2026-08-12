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
use std::sync::atomic::{AtomicBool, Ordering};
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

use crate::db::ThreadSummary;
use crate::language_model::{MessageContent, TokenUsage};
use crate::message::Message;
use crate::permission::{PendingAuthMeta, ToolAuthorizationResponse};
use crate::pi_approval::{ApprovalGate, ApprovalGatedTool, PiAskUserQuestionTool};
use crate::thread::{ApprovalMode, ThreadEvent};
use crate::thread_engine::{BackendNotice, SpawnedEngine, ThreadEngine};

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
    /// Switch the approval policy and persist it in the session sidecar.
    SetApprovalMode(ApprovalMode),
    /// Manual compaction (`/compact`), optionally steering the summary.
    Compact { custom_instructions: Option<String> },
    /// Toggle plan mode (persisted sidecar + hooks + instruction injection).
    SetPlanMode { enabled: bool },
    /// Persist whether a plan review card is pending (restore re-surfaces it).
    SetPlanReviewPending(bool),
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
    /// Close the session and stop the actor.
    Shutdown,
}

// BackendNotice is the shared facade/backend contract (thread_engine.rs);
// the actor sends it over the notice channel the facade drains.

/// Authoritative state the actor writes and the facade mirrors.
struct EngineState {
    running: AtomicBool,
    history: Mutex<Vec<Message>>,
    request_usage: Mutex<HashMap<String, TokenUsage>>,
    cumulative: Mutex<TokenUsage>,
    per_model: Mutex<HashMap<String, TokenUsage>>,
    /// USD cost aggregated from the kernel's rate-card pricing (#418 wire
    /// boundary costing); 0 until the session carries priced usage.
    cumulative_cost: Mutex<f64>,
    per_model_cost: Mutex<HashMap<String, f64>>,
    /// Shared with the approval gate so `SetModel` is visible to the
    /// reviewer without a second synchronization point.
    model: Arc<Mutex<Option<PiModel>>>,
    sessions: Mutex<Vec<ThreadSummary>>,
    active_path: Mutex<Option<PathBuf>>,
    /// Plan-mode state shared by the actor, the hooks, the gate, and the
    /// `ProposePlan` tool.
    plan: Arc<crate::plan_mode::PlanSessionState>,
    /// The host approval gate wrapping every tool (mode, always-allow
    /// cache, pending UI round trips).
    gate: Arc<ApprovalGate>,
    /// Shared goal state with the thread facade; the goal tools read/write
    /// through it, `GoalChanged` rides the notice channel. `None` when the
    /// threads db is unavailable (goal features degrade off).
    goal_bridge: Option<Arc<crate::goal_tools::GoalBridge>>,
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
) -> SpawnedEngine {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (notice_tx, notice_rx) = mpsc::unbounded_channel();
    let model_slot = Arc::new(Mutex::new(model.clone()));
    let gate = Arc::new(ApprovalGate::new(
        notice_tx.clone(),
        Arc::clone(&model_slot),
    ));
    let state = Arc::new(EngineState {
        running: AtomicBool::new(false),
        history: Mutex::new(Vec::new()),
        request_usage: Mutex::new(HashMap::new()),
        cumulative: Mutex::new(TokenUsage::default()),
        per_model: Mutex::new(HashMap::new()),
        cumulative_cost: Mutex::new(0.0),
        per_model_cost: Mutex::new(HashMap::new()),
        model: model_slot,
        sessions: Mutex::new(Vec::new()),
        active_path: Mutex::new(initial_path.clone()),
        gate,
        plan: crate::plan_mode::PlanSessionState::new(),
        goal_bridge,
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
    ));
    // Display-only streaming preview: while the actor's eager restore reads
    // the whole session file, stream its transcript into the mirrored history
    // in batches so the workspace paints the first messages early. The
    // authoritative `sync_history` at `Ready` replaces the preview.
    if let Some(path) = initial_path {
        spawn_history_preview(path, Arc::clone(&state), notice_tx);
    }
    SpawnedEngine {
        engine: Arc::new(PiEngine { cmd_tx, state }),
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
            let msgs: Vec<Message> = entries
                .iter()
                .flat_map(pi::session::session_entry_to_context_messages)
                .flat_map(|m| adapt::harness_messages_to_messages(std::slice::from_ref(&m)))
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

    fn history(&self) -> Vec<Message> {
        self.state.history.lock().unwrap().clone()
    }

    fn request_token_usage(&self) -> HashMap<String, TokenUsage> {
        self.state.request_usage.lock().unwrap().clone()
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

    fn set_model(&self, model: PiModel) {
        let _ = self.cmd_tx.send(SessionCmd::SetModel(model));
    }

    fn set_approval_mode(&self, mode: ApprovalMode) {
        let _ = self.cmd_tx.send(SessionCmd::SetApprovalMode(mode));
    }

    fn set_plan_mode(&self, enabled: bool) {
        let _ = self.cmd_tx.send(SessionCmd::SetPlanMode { enabled });
    }

    fn set_plan_review_pending(&self, pending: bool) {
        let _ = self.cmd_tx.send(SessionCmd::SetPlanReviewPending(pending));
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

/// Minimal builtin coding-agent prompt. Deliberately not the manox
/// `system_prompt` assembly — that belongs to the manox harness.
fn system_prompt(cwd: &Path) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d");
    let mut prompt = format!(
        "You are Manox Pi, a coding agent running inside the manox app on the pi harness.\n\
         Working directory: {cwd}\n\
         Date: {date}\n\n\
         Use your tools to inspect, edit, and create files and to run shell commands.\n\
         Make changes directly, keep replies concise, and verify your work when practical.",
        cwd = cwd.display(),
    );
    // Skill summaries let the model know which skills are installed (users
    // invoke them via `/name` slash commands). Parity with the retired manox
    // system prompt, which rendered `skill::summaries_or_empty()` into its
    // template; the pi path has no skill tool, so the wording only promises
    // what exists.
    let summaries = crate::skill::summaries_or_empty();
    if !summaries.is_empty() {
        prompt.push_str("\n\n## Available skills\n");
        prompt.push_str("Installed skills, invocable by the user as `/name` slash commands:\n");
        for s in &summaries {
            prompt.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
    }
    prompt
}

/// The full pi toolset: pi's file tools plus the pi-extensions bash/sub-agent
/// orchestration (assembly mirrors the `pi-extensions` orchestration example).
/// Every tool rides behind the host's [`ApprovalGatedTool`] (the kernel ships
/// no gate — approval policy is a harness concern); `AskUserQuestion` joins
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
) -> (Vec<Arc<dyn PiAgentTool>>, SessionOrchestrators) {
    // Bash execution backend: seatbelt-wrapped one-shot commands when the
    // OS backend is available (writes + network confined; shell state does
    // not persist — the tool's `cwd` parameter pins each call), otherwise
    // the unsandboxed persistent brush shell (approval-gated as always).
    // Background tasks ride the same policy: the registry's sandbox wrapper
    // reuses this backend's `wrap_command`, so a non-escalated background
    // task is confined exactly like a foreground call. Inside a worktree
    // session the policy is worktree-scoped: writable set narrowed to the
    // worktree, the bound repo's shared `.git` re-opened, network
    // unrestricted (a worktree is an approved isolation context).
    let sandbox_available = crate::sandbox::is_available();
    let mut background = Arc::new(BackgroundRegistry::new());
    let bash_ops: Arc<dyn pi::tools::bash::BashOperations> = if sandbox_available {
        let policy = worktree_policy(cwd, worktree);
        let ops = Arc::new(crate::sandbox::SandboxedBashOperations::new(cwd, policy));
        // Background tasks: wrap through the same policy/seatbelt as
        // foreground calls (the wrapper owns the allowlist proxy too).
        let wrap_ops = Arc::clone(&ops);
        let wrap: pi_extensions::bash::background::SandboxCommandBuilder =
            Arc::new(move |command, cwd| wrap_ops.wrap_background(command, cwd));
        background = Arc::new(BackgroundRegistry::new().with_sandbox(wrap));
        ops
    } else {
        Arc::new(PersistentShellOperations::new(cwd))
    };
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let monitor = Arc::new(MonitorManager::new(Arc::clone(&background)));
    // Escalation backend: no confinement at all. Selected per call when the
    // model passes `unsandboxed: true` or the host's force resolver
    // (Danger mode) says so — authorization is host policy, never the
    // model's word. Installed only where a seatbelt exists to escape from:
    // on other platforms the default backend is already unsandboxed, and
    // swapping it for a stateless one-shot would only discard shell state.
    let unsandboxed_ops: Option<Arc<dyn pi::tools::bash::BashOperations>> =
        sandbox_available.then(|| {
            let ops: Arc<dyn pi::tools::bash::BashOperations> =
                Arc::new(crate::sandbox::UnsandboxedBashOperations::new(cwd));
            ops
        });
    let force_gate = Arc::clone(gate);
    let force_unsandboxed = Arc::new(move || force_gate.mode() == ApprovalMode::Danger);
    let mut bash = BashTool::new(bash_ops, background.clone())
        .with_manager(Arc::clone(&manager))
        .with_sandbox_available(sandbox_available);
    if let Some(ops) = unsandboxed_ops {
        bash = bash.with_unsandboxed_operations(ops);
    }
    let bash = bash.with_force_unsandboxed(force_unsandboxed);

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
        Arc::new(pi::tools::find::FindTool),
        Arc::new(pi::tools::ls::LsTool),
        Arc::new(bash),
        Arc::new(MonitorTool::new(Arc::clone(&monitor))),
        Arc::new(BashOutputTool::new(background.clone())),
        Arc::new(TaskStopTool::new(background).with_ws_registry(monitor.ws_registry())),
        Arc::new(crate::web_fetch::WebFetchTool::new()),
    ];
    // Plan-mode gate exemption: plan-file writes stay approval-free while
    // plan mode is active (the `ToolCall` hook blocks everything else).
    let plan_policy = Arc::new(crate::plan_mode::PlanGatePolicy {
        state: Arc::clone(plan),
        plans_dir: crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans")),
        cwd: cwd.to_path_buf(),
    });
    // Sandboxed bash rides the OS confinement and skips the gate in
    // AutoPilot — the old-harness "sandboxed bash needs no approval"
    // semantics. Escalated calls (`unsandboxed: true`) still gate; Danger
    // already delegates; platforms without a seatbelt never auto-allow.
    let bash_auto_allow: Option<crate::pi_approval::AutoAllowResolver> =
        sandbox_available.then(|| {
            let gate = Arc::clone(gate);
            let allow: crate::pi_approval::AutoAllowResolver =
                Arc::new(move |name: &str, params: &serde_json::Value| {
                    name == "Bash"
                        && gate.mode() == ApprovalMode::AutoPilot
                        && !params["unsandboxed"].as_bool().unwrap_or(false)
                });
            allow
        });
    let mut tools: Vec<Arc<dyn PiAgentTool>> = tools
        .into_iter()
        .map(|tool| {
            let name = tool.name().to_string();
            let mut wrapper = ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy));
            if let Some(allow) = &bash_auto_allow
                && name == "Bash"
            {
                wrapper = wrapper.with_auto_allow(Arc::clone(allow));
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
    // Team coordination tools: every session registers the full set; the
    // facade routes ops through the calling thread's team (leader or
    // member), and ops without a team return clean errors. Write-axis
    // governance rides the thread's approval mode like any other tool.
    tools.push(Arc::new(crate::team::tools::TeamCreateTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::team::tools::TeamSpawnTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::team::tools::TeamDisbandTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::team::tools::SendMessageTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::team::tools::TaskCreateTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::team::tools::TaskListTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::team::tools::TaskUpdateTool::new(
        notice_tx.clone(),
    )));
    tools.push(Arc::new(crate::team::tools::TaskGetTool::new(
        notice_tx.clone(),
    )));
    // Goal lifecycle tools (GetGoal/CreateGoal/UpdateGoal): ungated like
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
    // axis stays ungated; the write axis rides the same approval gate as
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
    // Git worktree management: the tools run the git phase and queue the
    // session swap (actor-side, between turns); both approval-gated.
    tools.push(Arc::new(crate::worktree::EnterWorktreeTool::new(
        cmd_tx.clone(),
        Arc::clone(worktree),
    )));
    tools.push(Arc::new(crate::worktree::ExitWorktreeTool::new(
        cmd_tx.clone(),
        Arc::clone(worktree),
    )));
    // LSP code-intel tools: read-only, ride ungated. Registered once the
    // registry probe landed and at least one server spec is available;
    // otherwise the agent degrades to grep/glob (no LSP on PATH).
    if let Some(reg) = lsp::registry::try_global()
        && !reg.available_specs().is_empty()
    {
        tools.extend(crate::lsp_tools::tools());
    }
    // MCP servers (mcp.toml + plugin .mcp.json): each advertised tool rides
    // behind the same approval gate as built-ins (remote calls are mutating
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
    // The sub-agent tool needs a concrete model; a session assembled before
    // registration landed (first seconds after launch) skips it.
    if let Some(model) = model {
        let mut registry = AgentRegistry::new();
        register_defaults(&mut registry);
        // User-authored (~/.config/cx/manox/agents) + plugin-provided
        // (`<plugin>/agents/`, namespaced) definitions layer over the
        // built-ins; same-name user files override built-ins.
        crate::agent_defs::register_user_and_plugin(&mut registry);
        let subagent = SubagentTool::new(
            Arc::new(registry),
            vec![
                Arc::new(pi_extensions::read::SelectorReadTool::new()),
                Arc::new(pi::tools::grep::GrepTool),
                Arc::new(pi::tools::find::FindTool),
                Arc::new(pi::tools::ls::LsTool),
            ],
        )
        .with_model_runtime(runtime.clone())
        .with_model(model.clone())
        // Resolve agent-definition `model` overrides against the live
        // registry (registration has landed before session assembly).
        .with_provider_registry(crate::pi_providers::global());
        tools.push(Arc::new(subagent));
    }
    (
        tools,
        SessionOrchestrators {
            monitor,
            background: manager,
        },
    )
}

/// The sandbox policy for a session cwd: worktree-scoped when the session
/// runs inside the active worktree (writable set narrowed to the worktree,
/// the bound repo's shared `.git` re-opened, network unrestricted), else
/// project-scoped. The cwd comparison canonicalizes both sides so the
/// policy follows the session's actual directory — a stale pre-swap state
/// or an Exit back to the original repo both classify correctly.
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
        let main_git_dir =
            crate::sandbox::canonicalize_best_effort(&Path::new(&meta.original_cwd).join(".git"));
        crate::sandbox::SandboxPolicy::for_worktree(&wt, &main_git_dir)
    } else {
        crate::sandbox::SandboxPolicy::for_project(cwd)
    }
}

/// Session-scoped orchestrators that attach once a session exists: the
/// monitor manager (background command / WebSocket monitors) and the bash
/// background manager. Both are held by the session's tools as well, so the
/// managers live exactly as long as their session — a replaced session drops
/// its tools, and the monitor manager's `Drop` stops every monitor.
struct SessionOrchestrators {
    monitor: Arc<MonitorManager>,
    background: Arc<BackgroundManager>,
}

/// Bind the orchestrators to a freshly built session: the monitor steerer
/// lands events in the session's steering queue, and the background manager
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
                    // the actor's working model so the approval gate and
                    // settlement (title, session rebuilds) see it too.
                    handle.set_model(new_model.clone());
                    *state.model.lock().unwrap() = Some(new_model.clone());
                    *pi_model = new_model;
                }
                Some(_) => {} // queued prompts/reconfigs wait for settle
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
    title_state: &Arc<Mutex<TitleState>>,
    runtime: &ModelRuntime,
    pi_model: &PiModel,
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
    sync_history(session, sessions_dir, state).await;
    sync_usage(session, state).await;
    refresh_session_list(repo, state).await;
    let (steered, stranded) = if abort_requested || failed {
        (Vec::new(), std::mem::take(run_steers))
    } else {
        (std::mem::take(run_steers), Vec::new())
    };
    // A natural terminal turn may earn (or re-earn) the LLM title;
    // cancelled/failed turns keep the interim summary.
    if !abort_requested && !failed {
        maybe_generate_title(
            title_state,
            runtime,
            pi_model,
            session,
            sessions_dir,
            notice_tx,
        );
    }
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

/// Forward every pi run event through the adapt mapping onto the notice
/// channel as UI events.
fn subscribe_session(
    session: &AgentSession,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    live: Arc<Mutex<LiveTranscript>>,
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
            for te in adapt::agent_event_to_thread_events(&event) {
                let _ = tx.send(BackendNotice::Event(Box::new(te)));
            }
        })
    }))
}

/// Cheap change fingerprint for the live-history guard: message count plus
/// the trailing message's content size. Collisions only defer a facade
/// refresh by one tick, so exactness is unnecessary.
fn live_fingerprint(mapped: &[Message]) -> (usize, usize) {
    let trailing = mapped
        .last()
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
    (mapped.len(), trailing)
}

/// Refresh the engine's history mirror from the live transcript snapshot
/// (completed messages + the streaming partial). Returns whether the mirror
/// changed, so the caller can skip the facade notice on idle ticks (e.g. a
/// run parked on an approval verdict where nothing is streaming).
fn sync_live_history(live: &Arc<Mutex<LiveTranscript>>, state: &Arc<EngineState>) -> bool {
    let mut msgs: Vec<AgentMessage> = Vec::new();
    {
        let guard = live.lock().unwrap();
        msgs.extend(guard.messages.iter().cloned());
        if let Some(streaming) = &guard.streaming {
            msgs.push(streaming.clone());
        }
    }
    let mapped = adapt::harness_messages_to_messages(&msgs);
    let mut history = state.history.lock().unwrap();
    if live_fingerprint(&history) == live_fingerprint(&mapped) {
        return false;
    }
    *history = mapped;
    true
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
            clear_registry_displays_spawn(sessions_dir.clone(), session_path.clone());
            let _ = tx.send(BackendNotice::Event(Box::new(ThreadEvent::Compaction {
                summary: result.summary,
                messages_compacted: 0,
                tokens_before: result.tokens_before,
            })));
        }
        _ => {}
    }))
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
) -> (pi::coding_agent::AgentSessionBuilder, SessionOrchestrators) {
    let (tools, orchestrators) = build_tools(
        cwd,
        runtime,
        model,
        gate,
        plan,
        notice_tx,
        goal_bridge,
        cmd_tx,
        worktree,
    );
    let mut builder = create_agent_session()
        .with_cwd(cwd.to_path_buf())
        .with_session_dir(sessions_dir.to_path_buf())
        .with_model_runtime(runtime.clone())
        .with_system_prompt(system_prompt(cwd))
        .with_resources(instruction_resources(cwd))
        .with_tools(tools);
    if let Some(model) = model {
        builder = builder.with_model(model.clone());
    }
    (builder, orchestrators)
}

/// Register plan-mode extension hooks on a freshly built/restored session:
/// `BeforeAgentStart` injects the rendered plan-mode instructions every turn
/// while active; `ToolCall` enforces the read-only guarantee (plan-file
/// writes excepted). Both read through the shared [`PlanSessionState`].
fn attach_plan_hooks(
    session: &mut AgentSession,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    cwd: &Path,
) {
    session.on(
        pi::harness::HookPoint::BeforeAgentStart,
        crate::plan_mode::injection_handler(Arc::clone(plan)),
    );
    let plans_dir = crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans"));
    session.on(
        pi::harness::HookPoint::ToolCall,
        crate::plan_mode::gate_handler(Arc::clone(plan), plans_dir, cwd.to_path_buf()),
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

/// Register the FS path-policy hook: read tools (`Read`/`Grep`/`Find`/`Ls`)
/// are checked against the sensitive-path deny-list and write tools
/// (`Write`/`Edit`) against write confinement (project root + temp + plans
/// dir, `.git` protected). Block reasons surface to the model as tool
/// errors. Bash stays approval-gated only (no seatbelt in the pi slice).
///
/// Danger mode lifts write confinement — the user's highest authorization
/// level honors the settings panel's "edit any file" promise. Reads keep
/// the sensitive-path deny-list in every mode: a secret read is an
/// irreversible exfiltration even under full trust.
fn attach_path_policy_hooks(session: &mut AgentSession, cwd: &Path, gate: &Arc<ApprovalGate>) {
    let cwd = cwd.to_path_buf();
    let read_policy = Arc::new(crate::path_policy::ReadPolicy::new());
    let write_policy = Arc::new(crate::path_policy::WritePolicy::for_project(&cwd));
    let gate = Arc::clone(gate);
    session.on(
        pi::harness::HookPoint::ToolCall,
        Arc::new(move |mut ctx| {
            let danger = gate.mode() == ApprovalMode::Danger;
            let tool_name = ctx
                .data
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let args = ctx
                .data
                .get("args")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            let verdict =
                path_policy_verdict(tool_name, &args, &cwd, &read_policy, &write_policy, danger);
            if let Some(reason) = verdict {
                ctx.block_reason = Some(reason);
            }
            ctx
        }),
    );
}

/// Pure verdict for the path-policy hook: `Some(reason)` blocks the call.
/// Extracted from the hook closure so the danger/read/write matrix is
/// unit-testable without a live session.
fn path_policy_verdict(
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cwd: &Path,
    read_policy: &crate::path_policy::ReadPolicy,
    write_policy: &crate::path_policy::WritePolicy,
    danger: bool,
) -> Option<String> {
    match tool_name {
        "Read" | "Ls" | "Grep" | "Find" => args
            .get("path")
            .and_then(|v| v.as_str())
            .and_then(|raw| read_policy.check(&resolve_tool_path(raw, cwd)).err()),
        "Write" => {
            let target = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|raw| resolve_tool_path(raw, cwd));
            match target {
                Some(p) if !danger => write_policy.check(&p).err(),
                // Danger lifts write confinement but repo internals stay
                // protected: `.git` is repository structure, not a file the
                // "edit any file" promise covers (the c5aefe4d escape
                // class — a direct write into `.git` bypasses git itself).
                Some(p) if danger && has_git_component(&p) => Some(format!(
                    "Write blocked by path policy (`.git` is protected even in Danger): {}. \
                     Use git commands through bash (unsandboxed in Danger) for git state.",
                    p.display()
                )),
                _ => None,
            }
        }
        // Edit carries `{patch}` (hashline) with no top-level `path`;
        // confinement checks every `[path#TAG]` section target.
        "Edit" if !danger => args
            .get("patch")
            .and_then(|v| v.as_str())
            .and_then(|patch| write_policy.check_edit_patch(patch, cwd).err()),
        // Danger: same `.git`-only guard across every patch target.
        "Edit" if danger => args
            .get("patch")
            .and_then(|v| v.as_str())
            .and_then(|patch| {
                // Fail closed on unparseable patches, mirroring the
                // non-Danger arm — an unverifiable patch must not ride the
                // Danger release.
                let targets = match pi::hashline::parse_patch(patch) {
                    Ok(t) => t,
                    Err(e) => {
                        return Some(format!(
                            "Edit blocked by path policy (patch targets unverifiable even in Danger): {e}. \
                             Fix the hashline patch grammar and retry."
                        ));
                    }
                };
                let hit = targets.iter().find(|fp| {
                    let t = if fp.path.is_absolute() {
                        fp.path.clone()
                    } else {
                        cwd.join(&fp.path)
                    };
                    has_git_component(&t)
                });
                hit.map(|fp| {
                    format!(
                        "Edit blocked by path policy (`.git` is protected even in Danger): {}. \
                     Use git commands through bash (unsandboxed in Danger) for git state.",
                        fp.path.display()
                    )
                })
            }),
        _ => None,
    }
}

/// Whether a canonicalized path contains a `.git` component anywhere.
fn has_git_component(path: &Path) -> bool {
    crate::path_policy::canonicalize_best_effort(path)
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
}

/// Resolve a tool `path` argument against the session cwd (relative paths),
/// leaving absolute paths untouched.
fn resolve_tool_path(raw: &str, cwd: &Path) -> PathBuf {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
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
    // Reviewer side calls resolve their stream through this runtime.
    state.gate.set_runtime(runtime.clone());
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
            if let Some(requested) = &initial_path {
                return list.into_iter().find(|info| info.path == *requested);
            }
            list.into_iter().find(|info| info.message_count > 0)
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
        let (builder, orchestrators) = session_builder(
            &tool_cwd,
            &sessions_dir,
            &runtime,
            Some(&pi_model),
            &state.gate,
            &state.plan,
            &notice_tx,
            state.goal_bridge.as_ref(),
            &cmd_tx,
            &state.worktree,
        );
        match builder.open(info.path).await {
            Ok(mut s) => {
                attach_orchestrators(&mut s, &orchestrators);
                attach_plan_hooks(&mut s, &state.plan, &tool_cwd);
                attach_path_policy_hooks(&mut s, &tool_cwd, &state.gate);
                attach_plugin_hooks(&mut s, &tool_cwd);
                restored = true;
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
            let (builder, orchestrators) = session_builder(
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
            );
            // The fresh session carries the facade thread's id so the
            // sidebar row (keyed by session id) and the in-memory thread
            // share one identity.
            match builder.with_session_id(thread_id.clone()).build().await {
                Ok(mut s) => {
                    attach_orchestrators(&mut s, &orchestrators);
                    attach_plan_hooks(&mut s, &state.plan, &cwd);
                    attach_path_policy_hooks(&mut s, &cwd, &state.gate);
                    attach_plugin_hooks(&mut s, &cwd);
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
    let mut _subscription = subscribe_session(&session, &notice_tx, Arc::clone(&live_mirror));
    let mut _harness_subscription = subscribe_harness_events(
        &mut session,
        sessions_dir.clone(),
        session_path,
        &notice_tx,
        &wakeup_tx,
    );

    // The approval mode rides the session sidecar: restore it so a
    // reopened Danger session doesn't silently gate (or vice versa).
    let approval_mode = load_approval_mode(&sessions_dir, session.path()).await;
    state.gate.set_mode(approval_mode);
    // Plan mode rides the same sidecar: restore the flag so a reopened
    // planning session keeps its read-only gate; the facade re-renders and
    // re-sends the instructions once it sees `Ready`.
    let (plan_mode_restored, plan_file_restored) =
        load_plan_state(&sessions_dir, session.path()).await;
    if plan_mode_restored {
        state.plan.set(true, plan_file_restored.clone());
        state
            .plan
            .set_active_instructions(render_plan_instructions());
    }
    let plan_review_pending = load_plan_review_pending(&sessions_dir, session.path()).await;
    let mut title_state = load_title_state(&sessions_dir, session.path(), &session).await;

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
    let _ = notice_tx.send(BackendNotice::Ready {
        restored,
        model: Some(pi_model.clone()),
        approval_mode,
        plan_mode: plan_mode_restored,
        plan_file: plan_file_restored,
        plan_review_pending,
    });
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
                    )
                    .await;
                    settle_run(
                        &result,
                        abort_requested,
                        &session,
                        &state,
                        &repo,
                        &title_state,
                        &runtime,
                        &pi_model,
                        &sessions_dir,
                        &cwd,
                        &notice_tx,
                        &mut run_steers,
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
                // Drive the run while still servicing mid-run commands
                // (abort/steer) through the session handle.
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
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
                    &repo,
                    &title_state,
                    &runtime,
                    &pi_model,
                    &sessions_dir,
                    &cwd,
                    &notice_tx,
                    &mut run_steers,
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
            SessionCmd::SetApprovalMode(mode) => {
                state.gate.set_mode(mode);
                if let Err(err) =
                    write_approval_mode_sidecar(&sessions_dir, session.path(), mode).await
                {
                    tracing::warn!(error = %err, "failed to persist approval mode");
                }
            }
            SessionCmd::SetPlanMode { enabled } => {
                let plan_file = state.plan.plan_file();
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
                    write_plan_review_pending_sidecar(&sessions_dir, session.path(), pending).await
                {
                    tracing::warn!(error = %err, "failed to persist plan review pending flag");
                }
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
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
                    &repo,
                    &title_state,
                    &runtime,
                    &pi_model,
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
                        clear_registry_displays(&sessions_dir, session.path()).await;
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
                if let Err(err) = session.set_thinking_level(level).await {
                    tracing::warn!("pi set_thinking_level failed: {err}");
                }
            }
            SessionCmd::Open { path } => {
                rebuild_session(
                    &mut session,
                    &path,
                    &sessions_dir,
                    &runtime,
                    &pi_model,
                    &cwd,
                    &notice_tx,
                    &state.gate,
                    &state.plan,
                    state.goal_bridge.as_ref(),
                    &cmd_tx,
                    &state.worktree,
                )
                .await;
                _subscription = subscribe_session(&session, &notice_tx, Arc::clone(&live_mirror));
                _harness_subscription = subscribe_harness_events(
                    &mut session,
                    sessions_dir.to_path_buf(),
                    path.to_path_buf(),
                    &notice_tx,
                    &wakeup_tx,
                );
                resync_plan_state(&sessions_dir, &path, &state.plan, &notice_tx).await;
                resync_worktree_state(&sessions_dir, &path, &state.worktree, &notice_tx).await;
                *state.active_path.lock().unwrap() = Some(path);
                resync_approval_mode(&session, &sessions_dir, &state, &notice_tx).await;
                // Opened sessions are resumed conversations: SessionStart
                // already happened in a prior lifetime.
                state.session_start_fired.store(true, Ordering::SeqCst);
                title_state = load_title_state(&sessions_dir, session.path(), &session).await;
                sync_history(&session, &sessions_dir, &state).await;
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
            }
            SessionCmd::EnterWorktree {
                worktree_path,
                branch,
                original_cwd,
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
                // The worktree sidecar + shared state must be in place
                // BEFORE the session rebuild: `build_tools` derives the
                // sandbox policy from the active-worktree state
                // (worktree-scoped confinement with the bound repo's `.git`
                // re-opened). The WorktreeChanged notice fires early here;
                // the final refresh_session_list repaints everything.
                let meta = pi_extensions::session_meta::WorktreeMeta {
                    worktree_path: worktree_path.display().to_string(),
                    branch,
                    original_session_path: current_path.display().to_string(),
                    original_cwd: original_cwd.display().to_string(),
                };
                if let Err(err) =
                    write_worktree_sidecar(&sessions_dir, &fork_path, Some(meta)).await
                {
                    tracing::warn!(error = %err, "failed to persist worktree sidecar");
                }
                resync_worktree_state(&sessions_dir, &fork_path, &state.worktree, &notice_tx).await;
                rebuild_session(
                    &mut session,
                    &fork_path,
                    &sessions_dir,
                    &runtime,
                    &pi_model,
                    &cwd,
                    &notice_tx,
                    &state.gate,
                    &state.plan,
                    state.goal_bridge.as_ref(),
                    &cmd_tx,
                    &state.worktree,
                )
                .await;
                _subscription = subscribe_session(&session, &notice_tx, Arc::clone(&live_mirror));
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
                title_state = load_title_state(&sessions_dir, session.path(), &session).await;
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
                    &pi_model,
                    &cwd,
                    &notice_tx,
                    &state.gate,
                    &state.plan,
                    state.goal_bridge.as_ref(),
                    &cmd_tx,
                    &state.worktree,
                )
                .await;
                _subscription = subscribe_session(&session, &notice_tx, Arc::clone(&live_mirror));
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
                *state.active_path.lock().unwrap() = Some(original);
                resync_approval_mode(&session, &sessions_dir, &state, &notice_tx).await;
                title_state = load_title_state(&sessions_dir, session.path(), &session).await;
                sync_history(&session, &sessions_dir, &state).await;
                sync_usage(&session, &state).await;
                refresh_session_list(&repo, &state).await;
            }
            SessionCmd::NewSession { cwd, project } => {
                let (builder, orchestrators) = session_builder(
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
                );
                // Same identity contract as the startup build: the session
                // carries the facade thread's id (the previous deferred
                // session never materialized — `set_project` requires a
                // non-interacted thread).
                match builder.with_session_id(thread_id.clone()).build().await {
                    Ok(mut s) => {
                        attach_orchestrators(&mut s, &orchestrators);
                        attach_plan_hooks(&mut s, &state.plan, &cwd);
                        attach_path_policy_hooks(&mut s, &cwd, &state.gate);
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
                        let new_path = session.path().to_path_buf();
                        _subscription =
                            subscribe_session(&session, &notice_tx, Arc::clone(&live_mirror));
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
                        title_state = Arc::new(Mutex::new(TitleState::default()));
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
            SessionCmd::Shutdown => break,
        }
    }

    let _ = session.close().await;
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
    model: &PiModel,
    fallback_cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    gate: &Arc<ApprovalGate>,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    goal_bridge: Option<&Arc<crate::goal_tools::GoalBridge>>,
    cmd_tx: &mpsc::UnboundedSender<SessionCmd>,
    worktree: &crate::worktree::WorktreeState,
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
    let (builder, orchestrators) = session_builder(
        &cwd,
        sessions_dir,
        runtime,
        Some(model),
        gate,
        plan,
        notice_tx,
        goal_bridge,
        cmd_tx,
        worktree,
    );
    match builder.open(path.to_path_buf()).await {
        Ok(mut s) => {
            attach_orchestrators(&mut s, &orchestrators);
            attach_plan_hooks(&mut s, plan, &cwd);
            // Session swaps (Open/EnterWorktree/ExitWorktree) must carry
            // the same path policy as fresh builds — without it the
            // replaced session loses write confinement entirely.
            attach_path_policy_hooks(&mut s, &cwd, gate);
            attach_plugin_hooks(&mut s, &cwd);
            *session = s;
        }
        Err(err) => {
            let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
                "pi session open failed: {err}"
            )));
        }
    }
}

/// Actor-local title-generation state (manox `TitleState` parity): the
/// LLM title, the cadence anchor (user count at last evaluation), and the
/// in-flight lock. Shared with the spawned title task via `Arc<Mutex<_>>`.
#[derive(Default)]
struct TitleState {
    title: Option<String>,
    last_eval_user_count: Option<usize>,
    in_flight: bool,
}

/// Restore the title state from the session sidecar. `last_eval_user_count`
/// is derived from whether a title already exists, so a reloaded session
/// continues the cadence without re-evaluating immediately (manox parity).
async fn load_title_state(
    sessions_dir: &Path,
    session_path: &Path,
    session: &AgentSession,
) -> Arc<Mutex<TitleState>> {
    let meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .ok();
    let title = meta.and_then(|m| m.title).filter(|t| !t.trim().is_empty());
    let last_eval_user_count = title
        .is_some()
        .then(|| crate::title::count_user_messages(session.harness_messages()));
    Arc::new(Mutex::new(TitleState {
        title,
        last_eval_user_count,
        in_flight: false,
    }))
}

/// Maybe kick off an LLM title stream after a settled turn (manox
/// `maybe_generate_title` semantics): first title as soon as an assistant
/// reply exists, topic-shift re-eval on the cadence thereafter. The stream
/// runs in a spawned task (runtime resolver + `StreamFn`, reviewer-style)
/// and persists a landed title to the session sidecar.
fn maybe_generate_title(
    title_state: &Arc<Mutex<TitleState>>,
    runtime: &ModelRuntime,
    model: &PiModel,
    session: &AgentSession,
    sessions_dir: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    if !crate::settings::side_calls().title_policy().enabled {
        return;
    }
    let lang = crate::settings::load().resolve().agent;
    let (convo, user_count) = {
        let state = title_state.lock().unwrap();
        if state.in_flight {
            return;
        }
        let messages = session.harness_messages();
        let user_count = crate::title::count_user_messages(messages);
        if state.last_eval_user_count == Some(user_count) {
            return;
        }
        if state.title.is_some() && !crate::title::should_retitle(user_count) {
            return;
        }
        let Some(convo) =
            crate::title::build_title_messages(messages, state.title.as_deref(), lang)
        else {
            return;
        };
        (convo, user_count)
    };
    {
        let mut state = title_state.lock().unwrap();
        state.in_flight = true;
        state.last_eval_user_count = Some(user_count);
    }
    let runtime = runtime.clone();
    let model = model.clone();
    let sessions_dir = sessions_dir.to_path_buf();
    let session_path = session.path().to_path_buf();
    let tx = notice_tx.clone();
    let state = Arc::clone(title_state);
    crate::runtime::handle().spawn(async move {
        let result = crate::title::stream_title(&runtime, &model, convo).await;
        // Surface every outcome (manox parity): failures used to vanish into
        // a swallowed `if let Ok`, leaving the mechanical fallback with no
        // trace in the logs.
        match &result {
            Ok(title) if title.is_empty() => {
                tracing::warn!("title generation produced no usable text")
            }
            Ok(title) if crate::title::is_unchanged(title) => {
                tracing::debug!("title unchanged by model")
            }
            Ok(title) => tracing::debug!(title = %title, "title updated"),
            Err(e) => tracing::warn!(error = %format!("{e:?}"), "title generation stream failed"),
        }
        // Resolve the adoption under the lock, then persist outside it —
        // the guard must not span the sidecar awaits.
        let adopted = {
            let mut state = state.lock().unwrap();
            state.in_flight = false;
            let adopted = matches!(&result, Ok(title)
                if !title.is_empty() && !crate::title::is_unchanged(title));
            if adopted {
                state.title = result.as_ref().ok().cloned();
            }
            adopted
        };
        if adopted {
            let title = result.unwrap_or_default();
            let mut meta = pi_extensions::session_meta::load(&sessions_dir, &session_path)
                .await
                .unwrap_or_default();
            meta.title = Some(title);
            if let Err(err) =
                pi_extensions::session_meta::save(&sessions_dir, &session_path, &meta).await
            {
                tracing::warn!(error = %err, "failed to persist session title");
            }
            let _ = tx.send(BackendNotice::SessionListDirty);
        }
    });
}

/// The approval mode persisted in a session's sidecar; fresh sessions
/// (missing sidecar or field) default to AutoPilot.
async fn load_approval_mode(sessions_dir: &Path, session_path: &Path) -> ApprovalMode {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => meta
            .approval_mode
            .as_deref()
            .and_then(|raw| serde_json::from_value(serde_json::Value::String(raw.to_string())).ok())
            .unwrap_or_default(),
        Err(_) => ApprovalMode::default(),
    }
}

/// Persist the approval mode in the session sidecar so the session reopens
/// with the same gate policy.
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
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    meta.plan_mode = plan.enabled().then_some(true);
    meta.plan_file = plan.plan_file();
    pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await
}

async fn load_plan_review_pending(sessions_dir: &Path, session_path: &Path) -> bool {
    match pi_extensions::session_meta::load(sessions_dir, session_path).await {
        Ok(meta) => meta.plan_review_pending.unwrap_or(false),
        Err(_) => false,
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
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    meta.worktree = worktree;
    pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await
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
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    meta.plan_review_pending = pending.then_some(true);
    pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await
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

async fn write_approval_mode_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    mode: ApprovalMode,
) -> Result<(), anyhow::Error> {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    let raw = serde_json::to_value(mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "autopilot".to_string());
    meta.approval_mode = Some(raw);
    pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await
}

/// Re-read the session's persisted approval mode after a session switch and
/// align the gate + the facade's chip with it.
async fn resync_approval_mode(
    session: &AgentSession,
    sessions_dir: &Path,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    let mode = load_approval_mode(sessions_dir, session.path()).await;
    state.gate.set_mode(mode);
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::ApprovalModeChanged { mode },
    )));
}

/// Mirror the session's authoritative transcript into engine state, then
/// re-attach the registry slash turns' compact display forms from the
/// sidecar (the transcript stores only the expanded macro/skill body, so
/// the attach is what keeps a reloaded thread's bubbles compact).
async fn sync_history(session: &AgentSession, sessions_dir: &Path, state: &Arc<EngineState>) {
    let mut mapped = adapt::harness_messages_to_messages(session.harness_messages());
    attach_registry_displays(
        &mut mapped,
        &load_registry_displays(sessions_dir, session.path()).await,
    );
    *state.history.lock().unwrap() = mapped;
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

/// Drop the persisted registry display forms. A compaction rebuilds the
/// transcript as a summary user message plus the retained tail, which shifts
/// every persisted display ordinal; the stale forms would otherwise attach to
/// the wrong user prompt on the next `sync_history`. New registry turns after
/// the compaction persist fresh ordinals over the rebuilt sequence.
async fn clear_registry_displays(sessions_dir: &Path, session_path: &Path) {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    if meta.registry_displays.is_empty() {
        return;
    }
    meta.registry_displays.clear();
    if let Err(err) = pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await {
        tracing::warn!(error = %err, "failed to clear registry display text");
    }
}

/// `clear_registry_displays` from a harness event listener, which is
/// synchronous — the clear runs on the runtime and its outcome is only a
/// display form, so a lost write just narrows the reload window.
fn clear_registry_displays_spawn(sessions_dir: PathBuf, session_path: PathBuf) {
    tokio::spawn(async move {
        clear_registry_displays(&sessions_dir, &session_path).await;
    });
}

/// Re-attach `display_text` to the user prompt message at each persisted
/// ordinal. The ordinal counts `Role::User` messages with a `User`
/// provenance — the same set `Thread` counts when persisting — so steers
/// (user prompts too) and tool results (excluded) align between the two.
fn attach_registry_displays(
    history: &mut [Message],
    displays: &std::collections::HashMap<usize, String>,
) {
    if displays.is_empty() {
        return;
    }
    let mut ordinal = 0usize;
    for message in history {
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

/// Persist the bound project in the session sidecar so the sidebar groups
/// the session under its project folder across restarts.
async fn write_project_sidecar(sessions_dir: &Path, session_path: &Path, project: &Path) {
    let mut meta = pi_extensions::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    meta.project = Some(project.to_string_lossy().to_string());
    if let Err(err) = pi_extensions::session_meta::save(sessions_dir, session_path, &meta).await {
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
    *state.request_usage.lock().unwrap() = request_attribution(session);
}

/// Per-request attribution for the env card: the assistant usage of the
/// transcript, attributed to the triggering (most recent) user message.
/// Presentation-layer accounting, hence host-side.
fn request_attribution(session: &AgentSession) -> HashMap<String, TokenUsage> {
    let mut request: HashMap<String, TokenUsage> = HashMap::new();
    let key = last_user_id(session);
    for m in session.harness_messages() {
        let AgentMessage::Assistant { usage, .. } = m else {
            continue;
        };
        let u = to_token_usage(usage);
        if u.total_tokens() == 0 {
            continue;
        }
        request
            .entry(key.clone())
            .and_modify(|acc| *acc = *acc + u)
            .or_insert(u);
    }
    request
}

/// Fallback aggregation when `session_stats()` is unavailable: assistant
/// usage only, keyed by model id (the pre-stats mechanism).
fn sync_usage_from_messages(session: &AgentSession, state: &Arc<EngineState>) {
    let mut cumulative = TokenUsage::default();
    let mut per_model: HashMap<String, TokenUsage> = HashMap::new();
    for m in session.harness_messages() {
        let AgentMessage::Assistant { usage, model, .. } = m else {
            continue;
        };
        let u = to_token_usage(usage);
        if u.total_tokens() == 0 {
            continue;
        }
        cumulative = cumulative + u;
        per_model
            .entry(model.clone())
            .and_modify(|acc| *acc = *acc + u)
            .or_insert(u);
    }
    *state.cumulative.lock().unwrap() = cumulative;
    *state.per_model.lock().unwrap() = per_model;
    *state.cumulative_cost.lock().unwrap() = 0.0;
    *state.per_model_cost.lock().unwrap() = HashMap::new();
    *state.request_usage.lock().unwrap() = request_attribution(session);
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

/// The message id of the most recent user message, as the facade's history
/// assigns it. Usage is attributed to the turn's triggering user message.
fn last_user_id(session: &AgentSession) -> String {
    let mapped = adapt::harness_messages_to_messages(session.harness_messages());
    mapped
        .iter()
        .rev()
        .find(|m| matches!(m.role, crate::language_model::Role::User))
        .map(|m| m.id.clone())
        .unwrap_or_default()
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
            out.push(session_info_to_summary(&info));
        }
    }
    *state.sessions.lock().unwrap() = out;
}

/// Map a pi session info onto the sidebar summary shape.
fn session_info_to_summary(info: &pi::session::repository::SessionInfo) -> ThreadSummary {
    ThreadSummary {
        id: info.id.clone(),
        summary: info.first_message.clone(),
        title: None,
        title_override: None,
        model_id: String::new(),
        provider_id: None,
        approval_mode: 0,
        project: if info.cwd == "/" {
            String::new()
        } else {
            info.cwd.clone()
        },
        depth: 0,
        parent_id: info.parent_session_path.clone(),
        archived: false,
        pinned: false,
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
pub(crate) mod adapt {
    use super::*;
    use crate::language_model::{
        LanguageModelToolResult, LanguageModelToolUse, StopReason as ManoxStopReason,
    };
    use crate::thread::ToolCallStatus;
    use pi::types::StopReason as PiStopReason;

    /// Map one pi `AgentEvent` onto the `ThreadEvent`s the workspace renders.
    ///
    /// Events with no UI counterpart (run/turn lifecycle handled by the facade,
    /// message boundaries, block start/end markers) map to nothing.
    /// `ToolCallAuthorization` never comes from this mapping — the approval
    /// gate (`pi_approval`) emits it directly while parked on a verdict.
    /// `Plan*` and sub-agent events remain manox-only and are never produced.
    pub fn agent_event_to_thread_events(event: &AgentEvent) -> Vec<ThreadEvent> {
        match event {
            AgentEvent::AgentStart | AgentEvent::AgentEnd { .. } => Vec::new(),
            // `TurnStarted` is emitted once by the facade (matching `Thread`
            // semantics); pi's per-round `TurnStart` must not duplicate it.
            AgentEvent::TurnStart => Vec::new(),
            AgentEvent::MessageStart { .. } => Vec::new(),
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event {
                pi::types::AssistantMessageEvent::TextDelta { delta, .. } => {
                    vec![ThreadEvent::AgentText(delta.clone())]
                }
                pi::types::AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    vec![ThreadEvent::AgentThinking(delta.clone())]
                }
                _ => Vec::new(),
            },
            AgentEvent::MessageEnd { message } => match message_stop_reason(message) {
                Some(PiStopReason::Stop) => vec![ThreadEvent::Stop(ManoxStopReason::EndTurn)],
                Some(PiStopReason::Length) => vec![ThreadEvent::Stop(ManoxStopReason::MaxTokens)],
                Some(PiStopReason::ToolUse) => vec![ThreadEvent::Stop(ManoxStopReason::ToolUse)],
                Some(PiStopReason::Aborted) => {
                    vec![ThreadEvent::Stop(ManoxStopReason::Cancelled)]
                }
                Some(PiStopReason::Error) => {
                    vec![ThreadEvent::Error(anyhow::anyhow!(
                        "{}",
                        message_error_text(message)
                    ))]
                }
                None => Vec::new(),
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                let mut events = vec![ThreadEvent::ToolCall {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    title: tool_title(tool_name, arguments),
                    status: ToolCallStatus::Running,
                    input: Some(arguments.clone()),
                }];
                // A spawned sub-agent also lands as a rail observation row
                // (the conversation shows the Agent tool call card; the rail
                // tracks the nested session's lifecycle).
                if tool_name == crate::tools::AGENT {
                    events.push(ThreadEvent::SubagentProgress {
                        id: tool_call_id.clone(),
                        subagent_type: arguments
                            .get("subagent_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tool_uses: 0,
                        token_usage: crate::language_model::TokenUsage::default(),
                        latest_activity: arguments.get("prompt").and_then(|v| v.as_str()).map(
                            |prompt| {
                                let flat: String =
                                    prompt.split_whitespace().collect::<Vec<_>>().join(" ");
                                let mut chars = flat.chars();
                                let head: String = chars.by_ref().take(60).collect();
                                if chars.next().is_some() {
                                    format!("{head}…")
                                } else {
                                    head
                                }
                            },
                        ),
                        status: ToolCallStatus::Running,
                    });
                }
                events
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                partial_result,
                ..
            } => {
                // The Agent tool bridges its child session's streamed events
                // as `{"subagent_event": {...}}` progress: surface them as
                // drill-down transcript events + live rail activity.
                if tool_name == crate::tools::AGENT
                    && let Some(ev) = partial_result.get("subagent_event")
                {
                    let mut events = Vec::new();
                    let kind = ev.get("kind").and_then(|v| v.as_str()).unwrap_or_default();
                    let activity = |text: String| ThreadEvent::SubagentProgress {
                        id: tool_call_id.clone(),
                        subagent_type: String::new(),
                        tool_uses: 0,
                        token_usage: crate::language_model::TokenUsage::default(),
                        latest_activity: Some(text),
                        status: ToolCallStatus::Running,
                    };
                    match kind {
                        "text" => {
                            if let Some(text) = ev.get("text").and_then(|v| v.as_str()) {
                                events.push(ThreadEvent::SubagentChild {
                                    id: tool_call_id.clone(),
                                    child: crate::thread::SubagentChildEvent::Text(
                                        text.to_string(),
                                    ),
                                });
                            }
                        }
                        "thinking" => {
                            if let Some(text) = ev.get("text").and_then(|v| v.as_str()) {
                                events.push(ThreadEvent::SubagentChild {
                                    id: tool_call_id.clone(),
                                    child: crate::thread::SubagentChildEvent::Thinking(
                                        text.to_string(),
                                    ),
                                });
                            }
                        }
                        "tool_start" => {
                            let name = ev.get("tool").and_then(|v| v.as_str()).unwrap_or_default();
                            let summary = ev
                                .get("summary")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            events.push(ThreadEvent::SubagentChild {
                                id: tool_call_id.clone(),
                                child: crate::thread::SubagentChildEvent::ToolStart {
                                    name: name.to_string(),
                                    summary: summary.clone(),
                                },
                            });
                            events.push(activity(match summary {
                                Some(s) => format!("▸ {name} {s}"),
                                None => format!("▸ {name}"),
                            }));
                        }
                        "tool_end" => {
                            let name = ev.get("tool").and_then(|v| v.as_str()).unwrap_or_default();
                            let is_error = ev
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            events.push(ThreadEvent::SubagentChild {
                                id: tool_call_id.clone(),
                                child: crate::thread::SubagentChildEvent::ToolEnd {
                                    name: name.to_string(),
                                    is_error,
                                },
                            });
                            events.push(activity(format!(
                                "{} {name}",
                                if is_error { "✗" } else { "✓" }
                            )));
                        }
                        _ => {}
                    }
                    return events;
                }
                // The pi-extensions bash tool streams `{"output": chunk}`
                // partials; surface them as live tool output. Other partial
                // shapes carry no renderable text.
                match partial_result.get("output").and_then(|v| v.as_str()) {
                    Some(chunk) if !chunk.is_empty() => vec![ThreadEvent::ToolOutput {
                        id: tool_call_id.clone(),
                        chunk: chunk.to_string(),
                    }],
                    _ => Vec::new(),
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let status = if *is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Success
                };
                let mut events = vec![
                    ThreadEvent::ToolCall {
                        id: tool_call_id.clone(),
                        name: tool_name.clone(),
                        title: tool_name.clone(),
                        status,
                        input: None,
                    },
                    ThreadEvent::ToolResult {
                        id: tool_call_id.clone(),
                        output: tool_result_text(result),
                        is_error: *is_error,
                    },
                ];
                // Close the sub-agent's rail observation row (the row itself
                // was created by the start event; empty type here is fine —
                // the upsert keeps the existing entry's fields).
                if tool_name == crate::tools::AGENT {
                    events.push(ThreadEvent::SubagentProgress {
                        id: tool_call_id.clone(),
                        subagent_type: String::new(),
                        tool_uses: 0,
                        token_usage: crate::language_model::TokenUsage::default(),
                        latest_activity: None,
                        status,
                    });
                }
                events
            }
            AgentEvent::Retry {
                attempt,
                max_attempts,
                delay,
                reason,
                detail,
            } => vec![ThreadEvent::Retry {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_secs: delay.as_secs(),
                reason: reason.clone(),
                detail: detail.clone(),
            }],
            // Turn boundaries are owned by the facade, which knows whether the
            // run was cancelled or failed and which steers stranded.
            AgentEvent::TurnEnd { .. } => Vec::new(),
        }
    }

    /// Restore mapping: pi harness history onto the `agent::Message` history
    /// the rebuild path (`build_items`) renders. Blocks map one-to-one;
    /// terminal error/abort states surface as a trailing assistant text note
    /// so a reloaded session shows why the last run stopped.
    pub fn harness_messages_to_messages(input: &[AgentMessage]) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        for m in input {
            match m {
                AgentMessage::User { content, .. } => {
                    let content: Vec<MessageContent> = content
                        .iter()
                        .map(content_block_to_message_content)
                        .collect();
                    out.push(Message::user_with_content(content));
                }
                AgentMessage::Assistant {
                    content,
                    stop_reason,
                    error_message,
                    ..
                } => {
                    let mut blocks: Vec<MessageContent> = content
                        .iter()
                        .map(content_block_to_message_content)
                        .collect();
                    // Plan blocks surface through the PlanReady review flow;
                    // strip them from the displayed transcript (the session
                    // jsonl keeps the raw text, manox parity).
                    for block in blocks.iter_mut() {
                        if let MessageContent::Text(text) = block {
                            *text = crate::proposed_plan::strip_proposed_plan_blocks(text);
                        }
                    }
                    if matches!(stop_reason, Some(PiStopReason::Error)) {
                        blocks.push(MessageContent::Text(format!(
                            "[turn failed: {}]",
                            error_message.as_deref().unwrap_or("unknown error")
                        )));
                    }
                    if matches!(stop_reason, Some(PiStopReason::Aborted)) {
                        blocks.push(MessageContent::Text("[turn aborted]".to_string()));
                    }
                    out.push(Message::assistant(blocks));
                }
                AgentMessage::BashExecution {
                    command, output, ..
                } => {
                    // Dedicated shell-record card comes later; inline for now.
                    out.push(Message::assistant(vec![MessageContent::Text(format!(
                        "$ {command}\n{output}"
                    ))]));
                }
                AgentMessage::Custom {
                    content, display, ..
                } => {
                    if *display {
                        let blocks: Vec<MessageContent> = content
                            .iter()
                            .map(content_block_to_message_content)
                            .collect();
                        if !blocks.is_empty() {
                            out.push(Message::assistant(blocks));
                        }
                    }
                }
                AgentMessage::ToolResult {
                    tool_call_id,
                    tool_name,
                    content,
                    is_error,
                    ..
                } => {
                    // Tool results live in `Role::User` messages per the wire
                    // contract `build_items` expects.
                    out.push(Message::user_with_content(vec![
                        MessageContent::ToolResult(LanguageModelToolResult {
                            tool_use_id: tool_call_id.clone(),
                            tool_name: tool_name.clone().into(),
                            is_error: *is_error,
                            content: text_of_blocks(content),
                        }),
                    ]));
                }
            }
        }
        out
    }

    /// One pi content block onto one manox content block.
    fn content_block_to_message_content(block: &ContentBlock) -> MessageContent {
        match block {
            ContentBlock::Text { text, .. } => MessageContent::Text(text.clone()),
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => MessageContent::Thinking {
                text: thinking.clone(),
                signature: signature.clone(),
            },
            ContentBlock::Image { data, mime_type } => MessageContent::Image {
                data: data.clone(),
                mime_type: mime_type.clone(),
            },
            ContentBlock::ToolUse {
                id,
                name,
                input,
                thought_signature,
            } => MessageContent::ToolUse(LanguageModelToolUse {
                id: id.clone(),
                name: name.clone().into(),
                raw_input: input.to_string(),
                input: input.clone(),
                is_input_complete: true,
                thought_signature: thought_signature.clone(),
            }),
        }
    }

    /// Concatenate the text blocks of a tool result for display.
    fn tool_result_text(result: &pi::tool::AgentToolResult) -> String {
        text_of_blocks(&result.content)
    }

    fn text_of_blocks(blocks: &[ContentBlock]) -> String {
        let mut out = String::new();
        for block in blocks {
            if let ContentBlock::Text { text, .. } = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    fn message_stop_reason(message: &AgentMessage) -> Option<PiStopReason> {
        match message {
            AgentMessage::Assistant { stop_reason, .. } => *stop_reason,
            _ => None,
        }
    }

    fn message_error_text(message: &AgentMessage) -> String {
        match message {
            AgentMessage::Assistant { error_message, .. } => error_message
                .clone()
                .unwrap_or_else(|| "the pi session hit an error".to_string()),
            _ => "the pi session hit an error".to_string(),
        }
    }

    /// Human-readable tool card title from the pi tool name + arguments.
    /// Falls back to the bare name for tools without a recognized target
    /// field.
    pub fn tool_title(name: &str, args: &serde_json::Value) -> String {
        let arg = |key: &str| -> Option<String> {
            args.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match name {
            "Read" | "Write" | "Ls" => match arg("path") {
                Some(path) => format!("{name} {path}"),
                None => name.to_string(),
            },
            "Edit" | "EditDiff" => match arg("path") {
                Some(path) => format!("Edit {path}"),
                None => "Edit".to_string(),
            },
            "Grep" => match (arg("pattern"), arg("path")) {
                (Some(pattern), Some(path)) => format!("Grep {pattern} {path}"),
                (Some(pattern), None) => format!("Grep {pattern}"),
                _ => "Grep".to_string(),
            },
            "Find" => match arg("pattern") {
                Some(pattern) => format!("Find {pattern}"),
                None => "Find".to_string(),
            },
            "Bash" => match arg("command") {
                Some(command) => format!("$ {command}"),
                None => "Bash".to_string(),
            },
            "BashOutput" => "BashOutput".to_string(),
            "TaskStop" => "TaskStop".to_string(),
            "Agent" => match arg("subagent_type") {
                Some(kind) => format!("Agent {kind}"),
                None => "Agent".to_string(),
            },
            _ => name.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_mode_sidecar_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-1.jsonl");

        // Fresh session: no sidecar -> default.
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            ApprovalMode::AutoPilot
        );

        write_approval_mode_sidecar(dir.path(), &session, ApprovalMode::Danger)
            .await
            .unwrap();
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            ApprovalMode::Danger
        );

        write_approval_mode_sidecar(dir.path(), &session, ApprovalMode::AutoPilot)
            .await
            .unwrap();
        assert_eq!(
            load_approval_mode(dir.path(), &session).await,
            ApprovalMode::AutoPilot
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
        let mut history = vec![
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
        ];
        attach_registry_displays(&mut history, &displays);

        assert_eq!(
            history[0]
                .ui
                .as_ref()
                .and_then(|ui| ui.display_text.as_deref()),
            Some("/gitwork:deliver fast")
        );
        assert!(history[1].ui.is_none(), "plain turn keeps no display text");
        assert!(
            history[2].ui.is_none(),
            "tool result never consumes a display ordinal"
        );
        assert!(
            history[3].ui.is_none(),
            "assistant turns never get display text"
        );
        assert_eq!(
            history[4]
                .ui
                .as_ref()
                .and_then(|ui| ui.display_text.as_deref()),
            Some("/healthz")
        );
    }

    #[tokio::test]
    async fn clear_registry_displays_drops_sidecar_ordinals() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-display-clear.jsonl");
        let meta = pi_extensions::session_meta::SessionMeta {
            registry_displays: [(0usize, "/gitwork:deliver fast".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        pi_extensions::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();

        clear_registry_displays(dir.path(), &session).await;

        let displays = load_registry_displays(dir.path(), &session).await;
        assert!(
            displays.is_empty(),
            "a compaction clears the stale display ordinals"
        );
    }

    #[tokio::test]
    async fn clear_registry_displays_is_noop_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sess-display-empty.jsonl");

        clear_registry_displays(dir.path(), &session).await;

        let displays = load_registry_displays(dir.path(), &session).await;
        assert!(displays.is_empty());
    }

    #[tokio::test]
    async fn approval_mode_sidecar_tolerates_unknown_values() {
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
            ApprovalMode::AutoPilot,
            "unknown persisted modes fall back to the default"
        );
    }

    #[tokio::test]
    async fn approval_mode_write_preserves_other_sidecar_fields() {
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

        write_approval_mode_sidecar(dir.path(), &session, ApprovalMode::Danger)
            .await
            .unwrap();

        let loaded = pi_extensions::session_meta::load(dir.path(), &session)
            .await
            .unwrap();
        assert_eq!(loaded.title.as_deref(), Some("my thread"));
        assert_eq!(loaded.project.as_deref(), Some("/tmp/proj"));
        assert_eq!(loaded.approval_mode.as_deref(), Some("danger"));
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
                    "subagent_event": { "kind": "tool_start", "tool": "Read", "summary": "src/main.rs" }
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

        let end =
            adapt::agent_event_to_thread_events(&pi::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                arguments: serde_json::json!({}),
                partial_result: serde_json::json!({
                    "subagent_event": { "kind": "tool_end", "tool": "Read", "is_error": true }
                }),
            });
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
            request_usage: Mutex::new(HashMap::new()),
            cumulative: Mutex::new(TokenUsage::default()),
            per_model: Mutex::new(HashMap::new()),
            cumulative_cost: Mutex::new(0.0),
            per_model_cost: Mutex::new(HashMap::new()),
            model: model_slot,
            sessions: Mutex::new(Vec::new()),
            active_path: Mutex::new(None),
            gate,
            plan: crate::plan_mode::PlanSessionState::new(),
            goal_bridge: None,
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
                &history[0].content[0],
                crate::language_model::MessageContent::Text(t) if t == "hi"
            ));
            assert!(matches!(
                &history[1].content[0],
                crate::language_model::MessageContent::Text(t) if t == "part"
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
            &history[1].content[0],
            crate::language_model::MessageContent::Text(t) if t == "partial-answer"
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

    #[test]
    fn path_policy_verdict_blocks_outside_writes_outside_danger() {
        let cwd = Path::new("/tmp/manox-policy-hook-proj");
        let read = crate::path_policy::ReadPolicy::new();
        let write = crate::path_policy::WritePolicy::for_project(cwd);
        let args = serde_json::json!({ "path": "/etc/manox-hook-x" })
            .as_object()
            .unwrap()
            .clone();
        let reason = path_policy_verdict("Write", &args, cwd, &read, &write, false).unwrap();
        assert!(reason.contains("outside"), "{reason}");
    }

    #[test]
    fn path_policy_verdict_danger_lifts_write_confinement_but_keeps_read_denylist() {
        let cwd = Path::new("/tmp/manox-policy-hook-proj");
        let read = crate::path_policy::ReadPolicy::new();
        let write = crate::path_policy::WritePolicy::for_project(cwd);
        // Danger: an out-of-project Write and an out-of-project Edit both pass.
        let write_args = serde_json::json!({ "path": "/etc/manox-hook-x" })
            .as_object()
            .unwrap()
            .clone();
        assert!(
            path_policy_verdict("Write", &write_args, cwd, &read, &write, true).is_none(),
            "danger Write passes"
        );
        let edit_args = serde_json::json!({ "patch": "*** Begin Patch\n[/etc/manox-hook-x#1A2B]\nDEL 1\n*** End Patch" })
            .as_object()
            .unwrap()
            .clone();
        assert!(
            path_policy_verdict("Edit", &edit_args, cwd, &read, &write, true).is_none(),
            "danger Edit passes"
        );
        // Repo internals stay protected even under Danger: `.git` is
        // repository structure, not a file the "edit any file" promise
        // covers (a direct write bypasses git itself).
        let git_write_args = serde_json::json!({ "path": cwd.join(".git/config") })
            .as_object()
            .unwrap()
            .clone();
        let reason =
            path_policy_verdict("Write", &git_write_args, cwd, &read, &write, true).unwrap();
        assert!(reason.contains(".git"), "{reason}");
        let git_edit_args = serde_json::json!({
            "patch": format!(
                "*** Begin Patch\n[{}/.git/config#1A2B]\nDEL 1\n*** End Patch",
                cwd.display()
            )
        })
        .as_object()
        .unwrap()
        .clone();
        let reason = path_policy_verdict("Edit", &git_edit_args, cwd, &read, &write, true).unwrap();
        assert!(reason.contains(".git"), "{reason}");
        // Reads keep the sensitive-path deny-list even under Danger: a
        // secret read is an irreversible exfiltration.
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let read_args = serde_json::json!({ "path": home.join(".ssh/id_rsa") })
            .as_object()
            .unwrap()
            .clone();
        let reason = path_policy_verdict("Read", &read_args, cwd, &read, &write, true).unwrap();
        assert!(reason.contains("sensitive"), "{reason}");
    }

    #[test]
    fn worktree_policy_scopes_to_the_active_worktree_and_falls_back() {
        let base = crate::sandbox::canonicalize_best_effort(&std::env::temp_dir())
            .join("manox-wt-policy-proj");
        let wt = base.join("wt");
        let state = crate::worktree::new_state();
        // No active worktree → project policy (anchor None).
        let p = worktree_policy(&wt, &state);
        assert!(p.worktree_anchor().is_none());
        // Active worktree matching the session cwd → worktree-scoped:
        // writable in the worktree, the bound repo's shared `.git`
        // re-opened, network unrestricted.
        *state.lock().unwrap() = Some(pi_extensions::session_meta::WorktreeMeta {
            worktree_path: wt.display().to_string(),
            branch: "b".into(),
            original_session_path: "x".into(),
            original_cwd: base.display().to_string(),
        });
        let p = worktree_policy(&wt, &state);
        assert_eq!(p.worktree_anchor(), Some(wt.as_path()));
        assert!(p.is_write_allowed(&wt.join("file")), "worktree writable");
        assert!(
            p.is_write_allowed(&base.join(".git/refs/head")),
            "bound repo .git re-opened"
        );
        assert!(matches!(
            p.network(),
            crate::sandbox::NetworkPolicy::Unrestricted
        ));
        // Session cwd back at the original repo (Exit) → project policy
        // again, not the stale worktree scope.
        let p2 = worktree_policy(&base, &state);
        assert!(p2.worktree_anchor().is_none());
    }

    #[test]
    fn danger_edit_fails_closed_on_unparseable_patch() {
        let cwd = Path::new("/tmp/manox-policy-hook-proj");
        let read = crate::path_policy::ReadPolicy::new();
        let write = crate::path_policy::WritePolicy::for_project(cwd);
        // An unparseable hashline patch must not ride the Danger release —
        // the non-Danger arm already fails closed on it.
        let bad_args = serde_json::json!({ "patch": "[src/lib.rs#1A2B\nDEL 1" })
            .as_object()
            .unwrap()
            .clone();
        let reason = path_policy_verdict("Edit", &bad_args, cwd, &read, &write, true).unwrap();
        assert!(reason.contains("unverifiable"), "{reason}");
    }
}
