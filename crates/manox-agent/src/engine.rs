//! The pi harness engine: drives a `manox_harness::coding_agent::AgentSession` from a
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

use manox_harness::agents::{SubagentTool, register_defaults};
use manox_harness::bash::BashTool;
use manox_harness::bash::orchestration::BackgroundManager;
use manox_harness::bash::persistent::PersistentShellOperations;
use manox_harness::coding_agent::{AgentSession, ModelRuntime, create_agent_session};
use manox_harness::ext_point_agent::AgentRegistry;
use manox_harness::monitor::{MonitorManager, MonitorTool};
use manox_harness::tool::AgentTool as PiAgentTool;
use manox_harness::types::{AgentEvent, AgentMessage, ContentBlock, Model as PiModel};
use manox_harness::{BackgroundRegistry, BashOutputTool, TaskStopTool};
use tokio::sync::mpsc;

use crate::approval::{ApprovalGate, ApprovalGatedTool, PiAskUserQuestionTool};
use crate::db::{HistoryEntry, PositionedNote, ThreadSummary, UI_NOTE_CUSTOM_TYPE, UiNoteRecord};
use crate::language_model::{MessageContent, ReasoningEffort, TokenUsage};
use crate::message::Message;
use crate::permission::{PendingAuthMeta, ToolAuthorizationResponse};
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
        images: Vec<manox_harness::types::ContentBlock>,
        /// The RPC id the client submitted this turn under (dsh `source.rpcId`,
        /// §C.2 `originRpc`). The server pins it on the prompt's user-message
        /// journal entry so the client can retire its optimistic echo (§F.2).
        /// `None` for internally-driven turns (goal rounds, plan seeds).
        origin_rpc: Option<String>,
        /// K5: the journal entry id of the user message already persisted at
        /// Submit acceptance (`ThreadEngine::persist_user_submission`). The
        /// actor pins the middleware skip from it instead of appending its
        /// own entry; `None` persists the entry at drain, before the run.
        accepted_entry: Option<String>,
    },
    /// Inject a steer into the running turn.
    Steer {
        id: String,
        text: String,
        images: Vec<manox_harness::types::ContentBlock>,
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
    /// Create a fresh session in the given directory, optionally bound to a
    /// project (persisted in the session sidecar).
    NewSession {
        cwd: PathBuf,
        project: Option<PathBuf>,
    },
    /// Move the session's working directory (host-driven `SetCwd`): the
    /// sticky cwd advances and the move is durable as a `cwd_change`
    /// entry — never the header cwd or the project binding.
    SetCwd { path: PathBuf },
    /// Append a host UI annotation as a `custom` entry at the session leaf.
    /// The single actor queue makes send order the persist order, so a note
    /// dispatched before a prompt lands before the prompt's user entry.
    AppendUiNote(UiNoteRecord),
    /// Append a typed v4 journal entry (§C.2) at the session leaf. Fed by
    /// the notice-channel tap (`durable_journal_payload`): every durable
    /// `BackendNotice::Event` rides the same actor queue, so persist order
    /// equals notice order (L3/L4 — the journal covers every transition by
    /// construction, never by remembering call sites).
    AppendJournal {
        kind: String,
        payload: serde_json::Value,
    },
    /// Read the whole active chain + cursor (the follow-stream snapshot
    /// source, §C.3). Answered by the actor, which owns the session; parks
    /// mid-run like every other session-owned read.
    JournalSnapshot {
        reply: tokio::sync::oneshot::Sender<JournalSnapshotData>,
    },
    /// Close the session and stop the actor.
    Shutdown,
}

// BackendNotice is the shared facade/backend contract (thread_engine.rs);
// the actor sends it over the notice channel the facade drains.

/// The thread's journal feed as exposed to the host (session-core follow
/// streams subscribe to this). `Lagged` is the L5 resync signal: the
/// subscriber must re-open from a fresh snapshot, never assume silence.
#[derive(Debug, Clone)]
pub enum JournalFeed {
    Event(manox_harness::session::jsonl::JournalEvent),
    Lagged(u64),
}

/// One whole-chain journal read (§C.3), answered by the actor.
#[derive(Debug, Clone)]
pub struct JournalSnapshotData {
    pub cursor: u64,
    pub records: Vec<manox_harness::session::jsonl::JournalRecord>,
}

/// Relay a session's ordered storage journal broadcast into the
/// thread-scoped [`JournalFeed`] channel. One relay per live session; the
/// relay exits when the session's storage drops (a swap closes the channel).
fn spawn_journal_relay(
    session: &AgentSession,
    journal_tx: &tokio::sync::broadcast::Sender<JournalFeed>,
) {
    spawn_journal_relay_rx(session.subscribe_journal(), journal_tx.clone());
}

fn spawn_journal_relay_rx(
    mut rx: tokio::sync::broadcast::Receiver<manox_harness::session::jsonl::JournalEvent>,
    journal_tx: tokio::sync::broadcast::Sender<JournalFeed>,
) {
    crate::runtime::handle().spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = journal_tx.send(JournalFeed::Event(event));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let _ = journal_tx.send(JournalFeed::Lagged(n));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Answer a snapshot request from the session's journal read face.
///
/// Reads the storage directly through the shared appender (the same `Arc`
/// the persistence middleware holds), so the answer is current at any await
/// point — including mid-run, where live appends already land on the storage
/// under the append lock (L5 read face: reads never park behind the run).
async fn reply_journal_snapshot(
    appender: &JournalAppender,
    reply: tokio::sync::oneshot::Sender<JournalSnapshotData>,
) {
    let storage = appender.storage();
    // One locked read: the cursor is derived from the records themselves so
    // a concurrent append (the live serializer, the persistence middleware)
    // can never tear the pair — a records tail past the reported cursor
    // would fail the clients' snapshot validation (page tail == cursor,
    // §F.1 rule 1). `journal_range`'s last record IS the active leaf, so
    // this equals `journal_cursor` by construction (0 for an empty chain).
    let records = storage.journal_range(0, u64::MAX).await.unwrap_or_default();
    let cursor = records.last().map(|r| r.seq).unwrap_or(0);
    let _ = reply.send(JournalSnapshotData { cursor, records });
}

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
    /// Rows the mid-run serializer could not land (K4): after a fail-loud
    /// abort, later rows park here whole, and a permanent loss parks its
    /// durable `error` compensation record here when the storage itself is
    /// down. The settle and idle drains retry them in arrival order, so a
    /// record lands once the storage recovers.
    pending_journal: Mutex<Vec<(String, serde_json::Value)>>,
    /// The thread-scoped journal feed (storage broadcasts relayed into it,
    /// one relay per live session). Session-core follow streams subscribe.
    journal_tx: tokio::sync::broadcast::Sender<JournalFeed>,
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
    /// turn settles (e.g. `NewSession`/`Open`/`SetCwd` — the first two
    /// rebuild the session, the third moves the sticky cwd a running turn's
    /// tools are resolving against). drive_run
    /// parks them here instead of dropping; the idle loop drains and
    /// re-queues them so the post-settle cmd dispatch runs the handler.
    pending_session_cmds: Mutex<Vec<SessionCmd>>,
    /// The active session's shared journal appender, published on every
    /// session build/swap (K5): the Submit-acceptance path
    /// (`persist_user_submission`) appends the user entry durably through
    /// it from OUTSIDE the actor task, before the run exists. `None` only
    /// before the first session assembles — the actor's drain-time
    /// persistence covers a Submit accepted in that window.
    current_appender: Mutex<Option<Arc<JournalAppender>>>,
    /// K5 edge: the resources behind the acceptance-side expansion —
    /// published with the appender at every build/swap so
    /// `persist_user_submission` expands exactly like the run's
    /// `prompt_input` will.
    current_resources: Mutex<Option<manox_harness::harness::HarnessResources>>,
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
    /// Session-shared extra writable roots, derived per call from the
    /// effective cwd (same-repo worktree auto-admission + escalation
    /// accumulation). Feeds both the fs fence and the bash seatbelt.
    granted_roots: crate::granted_roots::GrantedRoots,
    /// The cwd last reported through a `CwdChanged` event. A settle emits
    /// one event per durable move, so the UI's directory display tracks the
    /// session tail without per-turn chatter.
    last_cwd_note: Mutex<Option<String>>,
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

/// Map a facade event to its durable journal entry (§C.2): `(wire kind,
/// payload)`. `None` means the event is not journaled — because it is
/// already persisted by its owning flow (`model_change`, `thinking_level`,
/// `cwd_change`, `compaction`, messages) or is snapshot-semantics
/// (`HistoryProgress/Restored`, `PlanReady`) or not in the vocabulary
/// (`PeerMessage`, steer bookkeeping — steer rides messages).
///
/// This is the single mapping point the notice tap consumes; payload keys
/// are the variant's camelCase serde names (envelope-key exclusivity §C.1:
/// handles are callId/agentId, never id).
fn durable_journal_payload(ev: &ThreadEvent) -> Option<(String, serde_json::Value)> {
    use serde_json::json;
    let status_str = |status: &crate::thread::ToolCallStatus| {
        serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
    };
    let stop_reason_str = |reason: &Option<crate::language_model::StopReason>| {
        reason.map(|r| match r {
            crate::language_model::StopReason::EndTurn => "end_turn",
            crate::language_model::StopReason::MaxTokens => "max_tokens",
            crate::language_model::StopReason::ToolUse => "tool_use",
            crate::language_model::StopReason::Refusal => "refusal",
            crate::language_model::StopReason::Cancelled => "cancelled",
        })
    };
    Some(match ev {
        // ── lifecycle ────────────────────────────────────────────────────
        ThreadEvent::TurnStarted => ("turn_start".into(), json!({})),
        ThreadEvent::TurnFinished {
            cancelled,
            failed,
            stranded_steer_ids,
        } => (
            "turn_finish".into(),
            json!({
                "cancelled": cancelled,
                "failed": failed,
                "strandedSteerIds": stranded_steer_ids,
            }),
        ),
        ThreadEvent::Stop(reason) => (
            "stop".into(),
            json!({ "reason": stop_reason_str(&Some(*reason)) }),
        ),
        ThreadEvent::Retry {
            attempt,
            max_attempts,
            delay_secs,
            reason,
            detail,
        } => (
            "retry".into(),
            json!({
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delaySecs": delay_secs,
                "reason": reason,
                "detail": detail,
            }),
        ),
        ThreadEvent::Error(err) => ("error".into(), json!({ "message": format!("{err:#}") })),
        // ── streaming deltas ─────────────────────────────────────────────
        ThreadEvent::AgentText(text) => ("agent_text_delta".into(), json!({ "delta": text })),
        ThreadEvent::AgentThinking(text) => {
            ("agent_thinking_delta".into(), json!({ "delta": text }))
        }
        ThreadEvent::ToolCall {
            id,
            name,
            title,
            status,
            input,
        } => (
            "tool_call".into(),
            json!({
                "callId": id,
                "name": name,
                "title": title,
                "status": status_str(status)?,
                "input": input,
            }),
        ),
        ThreadEvent::ToolResult {
            id,
            output,
            is_error,
        } => (
            "tool_result".into(),
            json!({ "callId": id, "output": output, "isError": is_error }),
        ),
        ThreadEvent::ToolOutput { id, chunk } => (
            "tool_output_chunk".into(),
            json!({ "callId": id, "chunk": chunk }),
        ),
        ThreadEvent::SubagentStarted {
            id,
            subagent_type,
            description,
            child,
        } => (
            "subagent_child".into(),
            json!({
                "agentId": id,
                "event": {
                    "type": "started",
                    "subagentType": subagent_type,
                    "description": description,
                    "childId": child.0,
                },
            }),
        ),
        ThreadEvent::SubagentProgress {
            id,
            subagent_type,
            tool_uses,
            token_usage,
            latest_activity,
            status,
            ..
        } => (
            "subagent_progress".into(),
            json!({
                "agentId": id,
                "agentType": subagent_type,
                "toolUses": tool_uses,
                "tokenUsage": serde_json::to_value(token_usage).unwrap_or(serde_json::Value::Null),
                "latestActivity": latest_activity,
                "status": status_str(status)?,
            }),
        ),
        ThreadEvent::SubagentChild { id, child } => (
            "subagent_child".into(),
            json!({
                "agentId": id,
                "event": serde_json::to_value(child).unwrap_or(serde_json::Value::Null),
            }),
        ),
        // ── state changes (sidecar writes continue during migration; the
        //    journal entry is the future single truth, L10) ───────────────
        ThreadEvent::PermissionModeChanged { mode } => (
            "permission_mode_change".into(),
            // The closed kebab wire vocabulary (§C.2 `mode`), never the
            // Debug name: the replay fold, the sidecar cache, and the wire
            // projection all parse `from_wire`.
            json!({ "mode": mode.wire() }),
        ),
        ThreadEvent::PlanModeChanged { enabled } => {
            ("plan_mode_change".into(), json!({ "enabled": enabled }))
        }
        ThreadEvent::PlanUpdated { snapshot } => (
            "plan_update".into(),
            json!({ "snapshot": serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null) }),
        ),
        ThreadEvent::GoalChanged { goal } => (
            "goal".into(),
            json!({ "goal": serde_json::to_value(goal).unwrap_or(serde_json::Value::Null) }),
        ),
        ThreadEvent::TitleChanged { title } => ("title".into(), json!({ "title": title })),
        ThreadEvent::BrowserSuitesChanged { suites } => (
            "browser_suites".into(),
            json!({
                "suites": serde_json::to_value(suites)
                    .unwrap_or(serde_json::Value::Null)
            }),
        ),
        ThreadEvent::BackgroundTaskUpdated { snapshot } => (
            "background_task".into(),
            json!({ "snapshot": serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null) }),
        ),
        ThreadEvent::ToolCallAuthorization {
            id,
            tool_name,
            summary,
            input,
        } => (
            "approval".into(),
            json!({
                "kind": "request",
                "authId": id,
                "payload": { "toolName": tool_name, "summary": summary, "input": input },
            }),
        ),
        ThreadEvent::CompactionStarted { tokens_before } => (
            "compaction_started".into(),
            json!({ "tokensBefore": tokens_before }),
        ),
        // ── metrics (low wire priority, still logged) ─────────────────────
        ThreadEvent::TokenUsageUpdated(usage) => (
            "metrics".into(),
            json!({
                "metricType": "token_usage",
                "data": serde_json::to_value(usage).unwrap_or(serde_json::Value::Null),
            }),
        ),
        ThreadEvent::PrefixStability {
            stability_pct,
            system_changed,
            tools_changed,
        } => (
            "metrics".into(),
            json!({
                "metricType": "prefix_stability",
                "data": { "stabilityPct": stability_pct, "systemChanged": system_changed, "toolsChanged": tools_changed },
            }),
        ),
        ThreadEvent::CacheInvalidation { reprocessed_tokens } => (
            "metrics".into(),
            json!({ "metricType": "cache_invalidation", "data": { "reprocessedTokens": reprocessed_tokens } }),
        ),
        ThreadEvent::SideCallMetricsUpdated(metrics) => (
            "metrics".into(),
            json!({
                "metricType": "side_call",
                "data": serde_json::to_value(metrics).unwrap_or(serde_json::Value::Null),
            }),
        ),
        ThreadEvent::MainCallMetricsUpdated(metric) => (
            "metrics".into(),
            json!({
                "metricType": "main_call",
                "data": serde_json::to_value(metric).unwrap_or(serde_json::Value::Null),
            }),
        ),
        // Already durable through their owning flows / not journaled.
        ThreadEvent::ModelChanged { .. }
        | ThreadEvent::ReasoningEffortChanged { .. }
        | ThreadEvent::CwdChanged { .. }
        | ThreadEvent::Compaction { .. }
        | ThreadEvent::PlanReady { .. }
        | ThreadEvent::HistoryProgress
        | ThreadEvent::HistoryRestored
        | ThreadEvent::SteerInjected { .. }
        | ThreadEvent::PeerMessage { .. } => return None,
    })
}

// ── Thread → engine journal routing (K3) ───────────────────────────────────
//
// Kernel-level decision points that live outside the engine actor (the
// thread store's pin/archive writes) must still journal through the actor's
// serializer queue (`SessionCmd::AppendJournal` → the K4 fail-loud typed
// append face), so their entries are linearized against every other writer
// of the same session. When no live engine holds the thread, the row
// cold-appends through a freshly opened storage — which is only safe while
// no live storage writes the same file. The retirement protocol below makes
// the handoff structural:
//
// - A dispatch under the registry lock either SENDS into a non-retired
//   route (the row is then guaranteed to be appended by the actor: its
//   shutdown claim drains every queued row under the same lock), or sees a
//   retired/absent route and takes the cold path.
// - The actor retires its route and claims the queue in one lock hold at
//   shutdown, appends the claimed rows, closes the session, and only then
//   (on exit) removes the route — so a waiting cold append starts strictly
//   after the live storage stopped writing. One writer at a time, no lost
//   row.
struct EngineRoute {
    tx: mpsc::UnboundedSender<SessionCmd>,
    /// Set (under the registry lock, together with the shutdown claim) when
    /// the actor broke its command loop. From then on dispatches never send
    /// into this route — they wait for its removal and cold-append.
    retired: Arc<std::sync::atomic::AtomicBool>,
}

static ENGINE_ROUTES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, EngineRoute>>> =
    std::sync::OnceLock::new();

fn engine_routes() -> &'static std::sync::Mutex<HashMap<String, EngineRoute>> {
    ENGINE_ROUTES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

pub(crate) fn register_engine_route(thread_id: &str, tx: &mpsc::UnboundedSender<SessionCmd>) {
    engine_routes().lock().unwrap().insert(
        thread_id.to_string(),
        EngineRoute {
            tx: tx.clone(),
            retired: Arc::new(AtomicBool::new(false)),
        },
    );
}

fn unregister_engine_route(thread_id: &str, tx: &mpsc::UnboundedSender<SessionCmd>) {
    let mut routes = engine_routes().lock().unwrap();
    if routes
        .get(thread_id)
        .is_some_and(|route| route.tx.same_channel(tx))
    {
        routes.remove(thread_id);
    }
}

/// K3 shutdown protocol, step 1: retire this thread's registry route and
/// claim every `AppendJournal` row already queued. Runs under the registry
/// lock, so a concurrent [`dispatch_store_journal_row`] either sent before
/// this point (its row is claimed here and appended before close) or sees
/// the retirement (it waits for the route's removal, then cold-appends
/// after this actor's storage stopped writing). Non-journal rows queued at
/// shutdown are dropped: the actor has broken its command loop.
fn retire_and_claim_journal_rows(
    thread_id: &str,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
) -> Vec<(String, serde_json::Value)> {
    let routes = engine_routes().lock().unwrap();
    if let Some(route) = routes.get(thread_id) {
        route.retired.store(true, Ordering::Relaxed);
    }
    let mut rows = Vec::new();
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCmd::AppendJournal { kind, payload } = cmd {
            rows.push((kind, payload));
        }
    }
    rows
}

/// Land one journal row outside the actor (shutdown claim or cold append);
/// the K4 fail-loud discipline applies. A permanent loss records its
/// durable `error` entry where the storage allows and logs loudly
/// otherwise — no facade is left to notify on these paths.
async fn append_row_fail_loud(
    appender: &JournalAppender,
    kind: String,
    payload: serde_json::Value,
) {
    if let Err(err) = append_typed_resilient(appender, &kind, payload).await {
        let _ = record_journal_loss(appender, &kind, &err).await;
        tracing::error!(%err, kind, "journal row could not land outside the actor loop");
    }
}

/// Route one store-level journal row (K3: pin/archive and any future
/// store-owned decision) to the thread's journal. Sends into the live
/// actor's serializer queue when one is registered; otherwise cold-appends
/// through a freshly opened storage once the file has no live writer.
/// Called synchronously by the store's dispatch; the waiting/cold paths run
/// on the agent runtime.
///
/// `session_path` is the store's cached journal-file path, needed for the
/// cold path; `None` with no live route logs and drops the row (a thread
/// that never materialized has no journal — its sidecar carries the flag
/// until the journal exists, the K2 fallback).
pub(crate) fn dispatch_store_journal_row(
    thread_id: String,
    session_path: Option<PathBuf>,
    kind: String,
    payload: serde_json::Value,
) {
    enum Fate {
        Queued,
        Wait,
        Cold,
    }
    let fate = {
        let routes = engine_routes().lock().unwrap();
        match routes.get(&thread_id) {
            Some(route) if !route.retired.load(Ordering::Relaxed) => {
                match route.tx.send(SessionCmd::AppendJournal {
                    kind: kind.clone(),
                    payload: payload.clone(),
                }) {
                    // The actor's shutdown claim covers this row: it drains
                    // every queued AppendJournal under the same lock that
                    // sets `retired`.
                    Ok(()) => Fate::Queued,
                    // Actor died without retiring (a Fatal early return):
                    // the route's removal is imminent — wait, then re-check.
                    Err(_) => Fate::Wait,
                }
            }
            // Retiring: the actor is appending its claimed rows / closing.
            Some(_) => Fate::Wait,
            None => Fate::Cold,
        }
    };
    match fate {
        Fate::Queued => {}
        Fate::Wait => {
            crate::runtime::handle().spawn(wait_then_cold_journal_append(
                thread_id,
                session_path,
                kind,
                payload,
            ));
        }
        Fate::Cold => {
            crate::runtime::handle().spawn(cold_journal_append(session_path, kind, payload));
        }
    }
}

/// Wait for a retiring route's removal (a successor engine re-registering
/// is re-checked and offered the row), then cold-append. Bounded: a hung
/// actor shutdown must not strand the decision forever — after the window
/// the row cold-appends best-effort with a loud log.
async fn wait_then_cold_journal_append(
    thread_id: String,
    session_path: Option<PathBuf>,
    kind: String,
    payload: serde_json::Value,
) {
    for _ in 0..200u32 {
        enum Step {
            Done,
            KeepWaiting,
            Cold,
        }
        let step = {
            let routes = engine_routes().lock().unwrap();
            match routes.get(&thread_id) {
                Some(route) if !route.retired.load(Ordering::Relaxed) => {
                    // A successor engine took over the thread: its actor
                    // serializes the row against the same session file.
                    match route.tx.send(SessionCmd::AppendJournal {
                        kind: kind.clone(),
                        payload: payload.clone(),
                    }) {
                        Ok(()) => Step::Done,
                        Err(_) => Step::KeepWaiting,
                    }
                }
                Some(_) => Step::KeepWaiting,
                None => Step::Cold,
            }
        };
        match step {
            Step::Done => return,
            Step::Cold => break,
            Step::KeepWaiting => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
        }
    }
    cold_journal_append(session_path, kind, payload).await;
}

/// Cold journal append for a decision whose thread has no live engine (K3):
/// open the session file and land the typed row through the same storage
/// face a live actor uses (`append_typed`: parent selection + append lock +
/// seq stamp + journal broadcast). Safe because the registry protocol
/// guarantees no live storage writes this file while the route is absent.
/// A thread whose journal never materialized has no file: the row is
/// skipped loudly at debug — the sidecar carries the flag until the journal
/// exists (the K2 fallback).
async fn cold_journal_append(
    session_path: Option<PathBuf>,
    kind: String,
    payload: serde_json::Value,
) {
    let Some(path) = session_path else {
        tracing::debug!(
            kind,
            "no session file to cold-append to; the sidecar carries the flag"
        );
        return;
    };
    if !path.exists() {
        tracing::debug!(kind, path = %path.display(), "session file does not exist; the sidecar carries the flag");
        return;
    }
    let storage = match manox_harness::session::jsonl::JsonlSessionStorage::open(&path).await {
        Ok(storage) => storage,
        Err(err) => {
            tracing::error!(%err, kind, path = %path.display(), "cold journal append could not open the session file");
            return;
        }
    };
    let session = manox_harness::session::Session::new(storage);
    append_row_fail_loud(&session, kind, payload).await;
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
    // K3: expose this engine's actor queue to the store-level decision
    // points (pin/archive) so their typed appends ride the same serializer.
    register_engine_route(&thread_id, &cmd_tx);
    // The journal tap (architecture §C, L3/L4): every BackendNotice funnels
    // through `notice_tx`; the tap forwards each one to the facade FIRST (UI
    // latency unchanged) and, for durable events, queues a typed journal
    // append onto the same actor command channel — persist order equals
    // notice order, and no future emission site can forget to persist.
    let (notice_tx, tap_rx) = mpsc::unbounded_channel();
    let (tap_tx, notice_rx) = mpsc::unbounded_channel();
    let tap_cmd_tx = cmd_tx.clone();
    crate::runtime::handle().spawn(async move {
        let mut tap_rx = tap_rx;
        while let Some(notice) = tap_rx.recv().await {
            if let BackendNotice::Event(event) = &notice
                && let Some((kind, payload)) = durable_journal_payload(event)
            {
                let _ = tap_cmd_tx.send(SessionCmd::AppendJournal { kind, payload });
            }
            let _ = tap_tx.send(notice);
        }
    });
    // The Steer bus is engine-scoped, not session-scoped: spawned-member
    // and live-subagent tracking must survive session swaps so a user
    // cancel always reaches every derivative this thread spawned.
    let bus = crate::steer_bus::AgentBus::new(thread_id.clone(), notice_tx.clone());
    let model_slot = Arc::new(Mutex::new(model.clone()));
    let gate = Arc::new(ApprovalGate::new(
        notice_tx.clone(),
        Arc::clone(&model_slot),
    ));
    // K3: the user's verdict on a parked card is an observable state
    // change — the gate journals it as an `approval` decision entry through
    // the actor queue (the request entry rides the notice tap).
    gate.set_journal_sink(cmd_tx.clone());
    // The thread-scoped journal feed; session relays publish into it as
    // sessions come and go (capacity matches the storage broadcast, L5).
    let (journal_feed_handle, _) = tokio::sync::broadcast::channel::<JournalFeed>(4096);
    let state = Arc::new(EngineState {
        running: AtomicBool::new(false),
        history: Mutex::new(Vec::new()),
        notes: Mutex::new(Vec::new()),
        notes_gen: AtomicU64::new(0),
        pending_ui_notes: Mutex::new(Vec::new()),
        pending_journal: Mutex::new(Vec::new()),
        journal_tx: journal_feed_handle.clone(),
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
        current_appender: Mutex::new(None),
        current_resources: Mutex::new(None),
        gate,
        plan: crate::plan_mode::PlanSessionState::new(),
        goal_bridge,
        goal_continuation_reserved: AtomicBool::new(false),
        goal_continuation_round: Mutex::new(None),
        session_start_fired: AtomicBool::new(false),
        granted_roots: crate::granted_roots::GrantedRoots::new(cwd.clone()),
        last_cwd_note: Mutex::new(None),
    });
    // The registry entry lives exactly as long as the actor: its exit
    // unregisters under a channel-identity guard (K3), so a store-level
    // decision after this point takes the cold-append path instead of
    // sending into a dead queue.
    let actor_cmd_tx = cmd_tx.clone();
    let registry_thread_id = thread_id.clone();
    let actor_notice_tx = notice_tx.clone();
    let actor_state = Arc::clone(&state);
    let actor_bus = Arc::clone(&bus);
    let actor_initial_path = initial_path.clone();
    crate::runtime::handle().spawn(async move {
        run_actor(
            cwd,
            model,
            sessions_dir,
            actor_initial_path,
            fresh,
            project,
            actor_cmd_tx.clone(),
            cmd_rx,
            actor_notice_tx,
            actor_state,
            thread_id,
            parent_session,
            actor_bus,
        )
        .await;
        unregister_engine_route(&registry_thread_id, &actor_cmd_tx);
    });
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
        let mut stream = match manox_harness::session_stream::SessionTranscriptStream::open(&path)
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
                .flat_map(manox_harness::session::session_entry_to_context_messages)
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
    fn run_with_origin(
        &self,
        prompt: String,
        images: Vec<manox_harness::types::ContentBlock>,
        origin: Option<String>,
        accepted_entry: Option<String>,
    ) {
        let _ = self.cmd_tx.send(SessionCmd::Prompt {
            text: prompt,
            images,
            origin_rpc: origin,
            accepted_entry,
        });
    }
    fn persist_user_submission(
        &self,
        text: &str,
        images: Vec<manox_harness::types::ContentBlock>,
        origin: Option<String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<String>, anyhow::Error>> + Send>,
    > {
        let state = Arc::clone(&self.state);
        let text = text.to_string();
        Box::pin(async move {
            // The actor publishes the active session's appender on every
            // build/swap; `None` means the session is not assembled yet —
            // the actor's drain-time persistence covers the Submit when its
            // Prompt cmd is processed (still before model-visible).
            let Some(appender) = state.current_appender.lock().unwrap().clone() else {
                return Ok(None);
            };
            // K5 edge: the accepted entry carries the POST-expansion text —
            // the same expansion the run's prompt_input announces — so the
            // middleware content-match skip holds and a slash-command
            // prompt never journals twice. Resources publish with the
            // appender; an unassembled session passes through raw and the
            // drain-time persistence expands under the same contract.
            let text = match state.current_resources.lock().unwrap().clone() {
                Some(resources) => manox_harness::harness::expand_prompt_with(&resources, &text),
                None => text,
            };
            let message = prompt_user_message(&text, &images);
            // Durable: a still-deferred session (no file yet) materializes
            // here — the accepted text is on disk before the caller's
            // receipt returns.
            let id = appender.append_message_durable(message, origin).await?;
            Ok(Some(id))
        })
    }
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

    fn subscribe_journal_feed(&self) -> tokio::sync::broadcast::Receiver<JournalFeed> {
        self.state.journal_tx.subscribe()
    }

    fn journal_snapshot(&self) -> tokio::sync::oneshot::Receiver<JournalSnapshotData> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.cmd_tx.send(SessionCmd::JournalSnapshot { reply: tx });
        rx
    }

    fn model(&self) -> Option<PiModel> {
        self.state.model.lock().unwrap().clone()
    }

    fn run(&self, prompt: String, images: Vec<manox_harness::types::ContentBlock>) {
        if let Some(title) = crate::title::initial_title(&prompt) {
            let _ = self.cmd_tx.send(SessionCmd::SetInitialTitle(title));
        }
        let _ = self.cmd_tx.send(SessionCmd::Prompt {
            text: prompt,
            images,
            origin_rpc: None,
            accepted_entry: None,
        });
    }

    fn steer(&self, text: String, images: Vec<manox_harness::types::ContentBlock>) -> String {
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

    fn set_cwd(&self, path: PathBuf) {
        let _ = self.cmd_tx.send(SessionCmd::SetCwd { path });
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
fn subagent_capability(def: &manox_harness::ext_point_agent::AgentDef) -> &'static str {
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

/// Host wrapper around `manox_harness::bash::BashTool` for the subagent
/// snapshot. A Sailor is already an async primitive, so a background bash
/// inside a subagent session is pointless AND dangerous — the subagent's
/// registry has no manager/seatbelt-wrap, so `run_in_background` would
/// spawn a bare process that bypasses the seatbelt, eludes
/// `TaskStop`/`BashOutput`/UI, and outlives the session (no `Drop` reap).
/// Reject `run_in_background` outright; the description is also corrected
/// for the subagent context (one-shot, no state persistence, no gating
/// claim) since the kernel BashTool's static text assumes the host session.
struct SubagentBashTool {
    inner: Arc<dyn manox_harness::tool::AgentTool>,
}

const SUBAGENT_BASH_DESCRIPTION: &str = "Execute a shell command. Each call runs in a fresh \
    one-shot shell at the current cwd (no persistent cwd/vars across calls), under the same \
    backend as the Captain (seatbelt-confined where the host has one). `run_in_background` is \
    not available inside a subagent — the subagent itself is the async primitive, so run long \
    commands in the foreground. Use `head_lines`/`tail_lines` to keep a selection of output \
    instead of piping through `head`/`tail`.";

#[async_trait::async_trait]
impl manox_harness::tool::AgentTool for SubagentBashTool {
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
    fn execution_mode(&self) -> manox_harness::tool::ExecutionMode {
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
        ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        if params["run_in_background"].as_bool().unwrap_or(false) {
            return Err(manox_harness::tool::ToolError::ExecutionFailed(
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
        ctx: &dyn manox_harness::tool::ToolContext,
        progress: &dyn manox_harness::tool::ToolProgress,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        if params["run_in_background"].as_bool().unwrap_or(false) {
            return Err(manox_harness::tool::ToolError::ExecutionFailed(
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

/// Host wrapper around `manox_harness::bash::TaskStopTool` that also stops
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
impl manox_harness::tool::AgentTool for LegacyAwareTaskStop {
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
        ctx: &dyn manox_harness::tool::ToolContext,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
        // Legacy tasks (Sailors) live in background_task; stop there first.
        if let Some(id) = params["task_id"].as_str()
            && crate::background_task::get_by_str(id).is_some()
        {
            crate::background_task::stop(id)
                .await
                .map_err(manox_harness::tool::ToolError::ExecutionFailed)?;
            return Ok(manox_harness::tool::AgentToolResult::text(format!(
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
        ctx: &dyn manox_harness::tool::ToolContext,
        _progress: &dyn manox_harness::tool::ToolProgress,
    ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
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

    /// The closed wire name (the §D.3 setter-note family carries it as a
    /// `String`, like every sibling setter; matches the serde `lowercase`
    /// representation the journal and ReadyInfo already serialize).
    pub fn wire(self) -> &'static str {
        match self {
            Self::ChromeUse => "chromeuse",
            Self::WebExplore => "webexplore",
        }
    }

    /// Parse a wire suite name; `None` for anything outside the closed
    /// vocabulary (the gateway answers an error note, never a panic).
    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "chromeuse" => Some(Self::ChromeUse),
            "webexplore" => Some(Self::WebExplore),
            _ => None,
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
    granted_roots: &crate::granted_roots::GrantedRoots,
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
    // mode reaches the seatbelt via the `mode_resolver`; the writable roots
    // follow the call's cwd through the shared granted-roots store.
    let sandbox_available = crate::sandbox::is_available();
    let mut background = Arc::new(BackgroundRegistry::new());
    // Shared per-call grant cell: an approved sandbox-escalation stamps the
    // wider mode here for exactly one call; the sandboxed backend's mode
    // resolver reads it before the standing session mode.
    let grant_cell: Arc<std::sync::atomic::AtomicI64> = Arc::new(
        std::sync::atomic::AtomicI64::new(manox_harness::sandbox::NO_GRANT),
    );
    let bash_ops: Arc<dyn manox_harness::tools::bash::BashOperations> = if sandbox_available {
        let sandbox_mode_gate = Arc::clone(gate);
        let cell_for_resolver = Arc::clone(&grant_cell);
        let sandbox_mode_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
            Arc::new(move || {
                let g = cell_for_resolver.load(std::sync::atomic::Ordering::SeqCst);
                if g != manox_harness::sandbox::NO_GRANT {
                    PermissionMode::from_i64(g)
                } else {
                    sandbox_mode_gate.mode()
                }
            });
        let ops = Arc::new(crate::sandbox::SandboxedBashOperations::new(
            cwd,
            granted_roots.clone(),
            sandbox_mode_resolver,
        ));
        let wrap_ops = Arc::clone(&ops);
        let wrap: manox_harness::bash::background::SandboxCommandBuilder =
            Arc::new(move |command, cwd| wrap_ops.wrap_background(command, cwd));
        background = Arc::new(BackgroundRegistry::new().with_sandbox(wrap));
        ops
    } else {
        Arc::new(PersistentShellOperations::new(cwd))
    };
    // Subagent snapshot inherits the same seatbelt backend so a Sailor's
    // Bash is confined exactly like the Captain's (B4: no ungated bypass).
    let subagent_bash_ops: Arc<dyn manox_harness::tools::bash::BashOperations> =
        Arc::clone(&bash_ops);
    let subagent_background = Arc::new(BackgroundRegistry::new());
    let manager = Arc::new(BackgroundManager::new(Arc::clone(&background)));
    let monitor = Arc::new(MonitorManager::new(Arc::clone(&background)));
    // Unsandboxed backend (no confinement): selected per call when the
    // effective mode is `danger-full-access` (the standing session mode, or
    // an approved `sandbox_permissions` grant). Installed only where a
    // seatbelt exists to escape from; on other platforms the default backend
    // is already unsandboxed.
    let unsandboxed_ops: Option<Arc<dyn manox_harness::tools::bash::BashOperations>> =
        sandbox_available.then(|| {
            let ops: Arc<dyn manox_harness::tools::bash::BashOperations> =
                Arc::new(crate::sandbox::UnsandboxedBashOperations::new(cwd));
            ops
        });
    // Standing-mode resolver (the session mode, NOT the grant — the grant is
    // what an escalation requests, so it cannot be its own baseline).
    let standing_gate = Arc::clone(gate);
    let standing_resolver: Arc<dyn Fn() -> PermissionMode + Send + Sync> =
        Arc::new(move || standing_gate.mode());
    let escalation_approver: Arc<dyn manox_harness::sandbox::EscalationApprover + Send + Sync> =
        Arc::new(crate::approval::GateEscalationApprover::new(Arc::clone(
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
        Arc::new(manox_harness::read::SelectorReadTool::new()),
        // Write/Edit carry the process write lock for their execution window:
        // concurrent writers to the same path get a named-holder conflict
        // instead of silently clobbering each other (old manox file_lock
        // semantics; owner stays "main" until the team system lands).
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(manox_harness::tools::write::WriteTool),
            "main",
        )),
        Arc::new(crate::file_lock::FileLockedTool::new(
            Arc::new(
                manox_harness::tools::edit::EditTool::default()
                    .with_enforce_seen_lines(crate::settings::edit().enforce_seen_lines),
            ),
            "main",
        )),
        Arc::new(manox_harness::tools::grep::GrepTool),
        Arc::new(manox_harness::tools::glob::GlobTool),
        Arc::new(manox_harness::tools::ls::LsTool),
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
    let confined_bash_auto_allow: Option<crate::approval::AutoAllowResolver> = sandbox_available
        .then(|| {
            let allow: crate::approval::AutoAllowResolver =
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
    // sees the shared granted roots so the fs fence widens exactly like the
    // seatbelt: same-repo worktree auto-admission plus escalation
    // accumulation, both derived from the call's effective cwd.
    let mut tools: Vec<Arc<dyn PiAgentTool>> = tools
        .into_iter()
        .map(|tool| {
            let name = tool.name().to_string();
            let mut wrapper = ApprovalGatedTool::new(tool, Arc::clone(gate))
                .with_plan_policy(Arc::clone(&plan_policy))
                .with_granted_roots(granted_roots.clone());
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
    // Task tools (TaskCreate/TaskList/TaskUpdate/TaskGet) were removed in
    // the tools-optimization cycle — they were retired with the Steer-based
    // team architecture and UpdatePlan provides a strictly better alternative.
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
    // LSP code-intel tools: read-only, ride ungated. Registered once the
    // registry probe landed and at least one server spec is available;
    // otherwise the agent degrades to grep/glob (no LSP on PATH).
    if let Some(reg) = lsp::registry::try_global()
        && !reg.available_specs().is_empty()
    {
        tools.extend(crate::lsp_tools::tools());
        // Pre-warm every detected LSP server at the session cwd so the
        // first code-intel call hits a ready server. Detached background
        // task — non-blocking, fire-and-forget.
        crate::lsp_tools::prewarm_background(cwd.to_path_buf());
    }
    // MCP servers (mcp.toml + plugin .mcp.json): each advertised tool rides
    // behind the same permission gate as built-ins (remote calls are mutating
    // by default). A registry that never initialized (pre-`manox_agent::init`
    // tests) contributes nothing.
    if let Some(registry) = crate::mcp::try_global() {
        for server in registry.servers() {
            for tool in &server.tools {
                let mcp_tool = Arc::new(crate::mcp::napi_tool::PiMcpTool::new(
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
        manox_harness::steer_bus::AgentId::Captain,
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
    let sailor_ctx: Arc<dyn manox_harness::tool::ToolContext> =
        Arc::new(manox_harness::tool::LocalToolContext::new(
            Arc::new(manox_harness::env::TokioExecutionEnv::new(
                cwd.to_path_buf(),
            )),
            cwd.to_path_buf(),
            Arc::new(manox_harness::tool::ToolState::new()),
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
        let provider_registry = crate::provider_glue::global();
        let subagent_bash_ops = Arc::clone(&subagent_bash_ops);
        let subagent_background = subagent_background.clone();
        let subagent_description = subagent_description.clone();
        move |model: &PiModel| {
            // Dedicated per-type models from the cx providers config's
            // `subagents:` map; an unreadable config warns and leaves
            // subagents inheriting the thread model.
            let overrides = manox_harness::provider::load_subagent_models(
                manox_harness::provider::default_config_path(),
            )
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "subagent model config unreadable; subagents inherit the thread model"
                );
                HashMap::new()
            });
            for key in overrides.keys() {
                if registry.get(key).is_none() {
                    tracing::warn!(
                        subagent_type = %key,
                        "subagent model config names an unknown subagent type"
                    );
                }
            }
            let subagent = SubagentTool::new(
                registry.clone(),
                vec![
                    Arc::new(manox_harness::read::SelectorReadTool::new()),
                    Arc::new(manox_harness::tools::grep::GrepTool),
                    Arc::new(manox_harness::tools::glob::GlobTool),
                    Arc::new(manox_harness::tools::ls::LsTool),
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
                            manox_harness::bash::BashTool::new(
                                Arc::clone(&subagent_bash_ops),
                                subagent_background.clone(),
                            )
                            .with_sandbox_available(sandbox_available),
                        ),
                    }),
                    Arc::new(crate::file_lock::FileLockedTool::new(
                        Arc::new(manox_harness::tools::write::WriteTool),
                        "sailor",
                    )),
                    Arc::new(crate::file_lock::FileLockedTool::new(
                        Arc::new(
                            manox_harness::tools::edit::EditTool::default()
                                .with_enforce_seen_lines(
                                    crate::settings::edit().enforce_seen_lines,
                                ),
                        ),
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
            // Resolve dedicated per-type model specs from the config map.
            .with_model_overrides(overrides)
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
async fn persist_ui_note(
    session: &AgentSession,
    state: &EngineState,
    notice_tx: &tokio::sync::mpsc::UnboundedSender<BackendNotice>,
    record: &UiNoteRecord,
) -> bool {
    let data = match serde_json::to_value(record) {
        Ok(value) => Some(value),
        Err(err) => {
            // A None payload renders as a ghost entry on reload; name the
            // impossible case loudly instead of silently dropping the card.
            tracing::warn!(error = %err, "UI note serialization failed");
            None
        }
    };
    // K9/K4 symmetry: the UI-note append is a typed-append face like any
    // other (plan/approval cards ride it) — bounded retries, then the
    // durable loss record (parked for the settle/idle drains when the
    // storage itself is down) and one facade notice. The former warn-only
    // path swallowed permanent losses: the card vanished on reload with no
    // journal trace. No mid-run cancel leg — a UI note is not transcript,
    // so its loss never voids the turn.
    let mut last_err = None;
    for attempt in 1..=TYPED_APPEND_ATTEMPTS {
        match session
            .append_custom(UI_NOTE_CUSTOM_TYPE, data.clone())
            .await
        {
            Ok(_) => return true,
            Err(err) => {
                tracing::warn!(%err, attempt, "UI note journal append failed");
                last_err = Some(err);
                if attempt < TYPED_APPEND_ATTEMPTS {
                    tokio::time::sleep(TYPED_APPEND_RETRY_DELAY * attempt).await;
                }
            }
        }
    }
    let err = last_err.expect("the attempt loop ran at least once");
    let appender = session.journal_appender();
    if let Some(row) = record_journal_loss(&appender, "ui_note", &err).await {
        state.pending_journal.lock().unwrap().push(row);
    }
    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
        anyhow::anyhow!(
            "journal append permanently failed for `ui_note`: {err:#}; the entry was dropped"
        ),
    ))));
    false
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

/// The shared mid-run journal writer (`AgentSession::journal_appender`): the
/// same session `Arc` the persistence middleware holds, so a mid-run append
/// is linearized by the session's append lock and broadcasts to followers
/// like any other append.
type JournalAppender =
    manox_harness::session::Session<manox_harness::session::jsonl::JsonlSessionStorage>;

/// The user message a prompt's text and images construct — the same shape
/// `prompt_input`'s batch entry carries (`[Text, images...]`), so the
/// middleware's content-match skip recognizes the run's announced message
/// as the already-persisted one.
fn prompt_user_message(text: &str, images: &[ContentBlock]) -> AgentMessage {
    let mut content = Vec::with_capacity(images.len() + 1);
    content.push(ContentBlock::Text {
        text: text.to_string(),
        signature: None,
    });
    content.extend(images.iter().cloned());
    AgentMessage::User {
        content,
        timestamp: chrono::Utc::now(),
    }
}

/// K5: resolve the prompt's user-entry persistence BEFORE the run starts.
///
/// A Submit accepted at the gateway arrives already persisted
/// (`accepted_entry` = the journal entry id appended at acceptance, origin
/// pinned on it); a queued Submit persists here, at drain — still ahead of
/// the run, so ahead of model-visible ("model-visible ⟺ logged"). Both arm
/// the middleware skip (entry id + accepted content) so the run's own user
/// `MessageEnd` records the existing entry instead of appending a
/// duplicate; jsonl's duplicate-entry-id refusal stays the backstop, never
/// the normal path. The pin and any drain-time append carry the
/// POST-expansion text (the K5 edge: the run announces the expanded
/// shape); the acceptance-side persistence expands identically. An origin-less prompt (an internally-driven `run()`
/// turn, a goal/monitor seed) keeps the legacy flow: the middleware
/// persists its user message at announce time.
async fn persist_prompt_user_entry(
    session: &AgentSession,
    text: &str,
    images: &[ContentBlock],
    origin_rpc: Option<String>,
    accepted_entry: Option<String>,
) -> Result<Option<String>, anyhow::Error> {
    let appender = session.journal_appender();
    // A stale pin no run consumed (one died before announcing its user
    // message) must not leak the middleware skip into this turn.
    appender.clear_accepted_user_entry();
    if accepted_entry.is_none() && origin_rpc.is_none() {
        return Ok(None);
    }
    // K5 edge: the pin carries the POST-expansion content (the shape the
    // run announces) and the queued-submit drain persistence logs the
    // expanded text the model actually sees. Run-time input-hook
    // transforms remain a residual edge: their mismatch double-journals
    // acceptably (the raw pin is the user intent, the appended announce
    // the model-visible truth).
    let expanded = manox_harness::harness::expand_prompt_with(session.resources(), text);
    let message = prompt_user_message(&expanded, images);
    let content = match &message {
        AgentMessage::User { content, .. } => {
            serde_json::to_value(content).unwrap_or(serde_json::Value::Null)
        }
        _ => unreachable!("prompt_user_message builds a user message"),
    };
    let entry_id = match accepted_entry {
        Some(id) => id,
        None => {
            // Durable: a still-deferred session (no file yet) puts the
            // accepted text on disk here, not at the first assistant
            // message.
            appender.append_message_durable(message, origin_rpc).await?
        }
    };
    appender.pin_accepted_user_entry(entry_id.clone(), content);
    Ok(Some(entry_id))
}

/// K4 (§C.3, L3): the typed-append discipline shared by every journal write
/// face. A transient failure retries a bounded number of times with a short
/// backoff; a permanent failure must never drop the row silently — callers
/// run the fail-loud tail ([`record_journal_loss`], a facade notice, and,
/// mid-run, turn cancellation), mirroring the persistence middleware's
/// message-append rule where a failure aborts the whole run.
const TYPED_APPEND_ATTEMPTS: u32 = 3;
const TYPED_APPEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

async fn append_typed_resilient(
    appender: &JournalAppender,
    kind: &str,
    payload: serde_json::Value,
) -> Result<String, anyhow::Error> {
    let mut last_err = None;
    for attempt in 1..=TYPED_APPEND_ATTEMPTS {
        match appender.append_typed(kind, payload.clone()).await {
            Ok(id) => return Ok(id),
            Err(err) => {
                tracing::warn!(%err, kind, attempt, "typed journal append failed");
                last_err = Some(err);
                if attempt < TYPED_APPEND_ATTEMPTS {
                    tokio::time::sleep(TYPED_APPEND_RETRY_DELAY * attempt).await;
                }
            }
        }
    }
    Err(last_err.expect("the attempt loop ran at least once"))
}

/// The K4 loss record for a permanently failed typed append: a durable
/// `error` entry naming the dropped kind and reason. Best-effort — when the
/// storage itself is down the record cannot land either, so it is handed
/// back for the caller to park (`pending_journal`) and the settle/idle drain
/// persists it once the storage recovers. `error` rows never spawn a
/// compensation of their own: that breaks the tap feedback loop
/// (`ThreadEvent::Error` → tap → `AppendJournal("error")` → loss → …) and
/// keeps a dead storage from amplifying one failure into a notice storm.
async fn record_journal_loss(
    appender: &JournalAppender,
    kind: &str,
    err: &anyhow::Error,
) -> Option<(String, serde_json::Value)> {
    if kind == "error" {
        tracing::error!(%err, "journal append failed for an error record; dropping it");
        return None;
    }
    let compensation = (
        "error".to_string(),
        serde_json::json!({
            "message": format!(
                "journal append permanently failed for `{kind}`: {err:#} — the entry was dropped"
            ),
        }),
    );
    match appender
        .append_typed(&compensation.0, compensation.1.clone())
        .await
    {
        Ok(_) => None,
        Err(_) => Some(compensation),
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
    handle: &manox_harness::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    pi_model: &mut PiModel,
    sessions_dir: &Path,
    session_path: &Path,
    appender: &Arc<JournalAppender>,
) -> (anyhow::Result<Vec<AgentMessage>>, bool)
where
    F: std::future::Future<Output = anyhow::Result<Vec<AgentMessage>>>,
{
    tokio::pin!(run);
    // Live journal appends run on a dedicated serializer task, never inline
    // in this select. The `run` branch below shares THIS task, and its
    // persistence middleware holds the session's append lock across file-I/O
    // await points (`append_message_with_origin` → `append_line`); a select
    // handler that awaits the same lock suspends the whole task, so the
    // select can never poll the run branch again to release it — a same-task
    // self-deadlock that froze the turn forever with the journal tail stuck
    // mid-round (the round-11 stall). The serializer keeps facade rows
    // ordered (FIFO channel, sequential appends) and stays linearized
    // against middleware appends by the same session append lock.
    let (live_row_tx, mut live_row_rx) = mpsc::unbounded_channel::<(String, serde_json::Value)>();
    let live_appender = Arc::clone(appender);
    let live_state = Arc::clone(state);
    let live_notice = notice_tx.clone();
    let live_handle = handle.clone();
    // K4 fail-closed flag: a permanent mid-run append loss cancels the turn;
    // drive_run folds this into `abort_requested` so settle reports it.
    let live_abort = Arc::new(AtomicBool::new(false));
    let live_abort_flag = Arc::clone(&live_abort);
    let live_appends = tokio::spawn(async move {
        let mut loss_signaled = false;
        while let Some((kind, payload)) = live_row_rx.recv().await {
            let Err(err) = append_typed_resilient(&live_appender, &kind, payload.clone()).await
            else {
                continue;
            };
            // Permanent loss (K4, L3): the state this row describes already
            // took effect, so the run must not silently continue without it.
            // The first loss records itself durably, notifies the facade,
            // and cancels the turn; later rows of the same dead-storage run
            // park for the settle/idle drain without re-signaling (one
            // notice, one abort — the turn is already converging).
            if loss_signaled {
                live_state
                    .pending_journal
                    .lock()
                    .unwrap()
                    .push((kind, payload));
                continue;
            }
            if let Some(row) = record_journal_loss(&live_appender, &kind, &err).await {
                live_state.pending_journal.lock().unwrap().push(row);
            }
            if kind != "error" {
                loss_signaled = true;
                let _ = live_notice.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                    anyhow::anyhow!(
                        "journal append permanently failed for `{kind}`: {err:#}; cancelling the turn"
                    ),
                ))));
                live_abort_flag.store(true, Ordering::SeqCst);
                live_handle.abort();
            }
        }
    });
    let mut abort_requested = false;
    let mut channel_open = true;
    let mut live_ticker = tokio::time::interval(LIVE_HISTORY_TICK);
    live_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the mirror is not
    // re-synced at run start (the settle/ready path already mirrored).
    live_ticker.tick().await;
    // Stall watchdog: a turn whose journal tail stops moving while the run
    // future stays pending is the round-11 stall shape (tools finished, no
    // durable rows, no settle). The warn carries the frozen cursor so the
    // log bracket around the stall is unmissable; it repeats every 2 minutes.
    let mut watchdog_cursor: Option<u64> = None;
    let mut watchdog_last_move = std::time::Instant::now();
    let appender_for_watchdog = appender;
    let result = loop {
        if !channel_open {
            break run.await;
        }
        tokio::select! {
            _ = live_ticker.tick() => {
                if sync_live_history(&live, state) {
                    let _ = notice_tx.send(BackendNotice::LiveHistory);
                }
                let cursor = appender_for_watchdog.storage().journal_cursor().await;
                if watchdog_cursor != Some(cursor) {
                    watchdog_cursor = Some(cursor);
                    watchdog_last_move = std::time::Instant::now();
                } else if watchdog_last_move.elapsed()
                    > std::time::Duration::from_secs(120)
                {
                    tracing::warn!(
                        session = ?session_path,
                        cursor,
                        elapsed_secs = watchdog_last_move.elapsed().as_secs(),
                        "turn stall watchdog: the journal tail has not moved while the run stays pending"
                    );
                    watchdog_last_move = std::time::Instant::now();
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
                    // Mid-run switch: the harness handle queues it for the
                    // next turn boundary (the kernel's TS mid-run `setModel`
                    // path), where the model_change entry persists. The
                    // mirrors follow the handle's verdict, not the request: a
                    // model the fixed stream refuses leaves every mirror on
                    // the model the run still serves, and the model already in
                    // play is never re-queued.
                    if *pi_model != new_model && handle.set_model(new_model.clone()) {
                        *state.model.lock().unwrap() = Some(new_model.clone());
                        *pi_model = new_model;
                    }
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
                    if let Err(error) = persist_title(sessions_dir, &target, title.clone()).await {
                        tracing::warn!(%error, "failed to persist Title agent result");
                    } else {
                        let _ = notice_tx.send(BackendNotice::SessionListDirty);
                        // The facade mirrors the persisted title so the
                        // title bar tracks the sidebar without a reload.
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::TitleChanged { title },
                        )));
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
                Some(SessionCmd::AppendJournal { kind, payload }) => {
                    // Forwarded to the serializer task spawned above — this
                    // arm must NEVER await the session's append lock inline:
                    // the run branch of this same select suspends holding it
                    // (the persistence middleware's file I/O), and a handler
                    // await would deadlock the task against itself (the
                    // round-11 stall). Rows still append LIVE through the
                    // shared session handle in emission order; parking for
                    // settle hid subagent/retry/background rows for the whole
                    // run (L5 — the round-8 repro: a dispatched Sailor's
                    // failure never reached the journal while the captain
                    // kept working).
                    let _ = live_row_tx.send((kind, payload));
                }
                Some(SessionCmd::JournalSnapshot { reply }) => {
                    // Answer live off the storage (same read face as the
                    // idle loop): parking froze GetConversationInfo and
                    // PageHistory — the Q face AND the follow streams' gap
                    // repair — for the entire duration of a running turn.
                    reply_journal_snapshot(appender, reply).await;
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
    // Drain the serializer before returning: every AppendJournal received
    // mid-run has landed (or parked for settle) before settle_run reads the
    // journal. The run future has finished, so the append lock is free and
    // the drain is bounded by the queued rows.
    drop(live_row_tx);
    let _ = live_appends.await;
    // A K4 fail-closed cancel (permanent journal loss) counts as the abort
    // it was: settle reports `cancelled` and strands the run's steers.
    if live_abort.load(Ordering::SeqCst) {
        abort_requested = true;
    }
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
    sessions_dir: &Path,
    cwd: &Path,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    run_steers: &mut Vec<String>,
) {
    let failed = result.is_err();
    // K5: a pin no run consumed (one died before announcing its user
    // message) must not leak the middleware skip into the next turn.
    session.journal_appender().clear_accepted_user_entry();
    if let Err(err) = result {
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
            anyhow::anyhow!("{err:#}"),
        ))));
    }
    state.running.store(false, Ordering::Relaxed);
    // The sticky cwd may have moved during the run (a tool call with an
    // explicit `cwd`, or a `cd` inside a command): report one `CwdChanged`
    // per durable move so the facade mirror and the UI track the session
    // tail. The projected cwd comes from the session path — the flush has
    // already made the move durable at this point.
    let projected = session.projected_cwd().await;
    let projected_str = projected.to_string_lossy().into_owned();
    let last = state.last_cwd_note.lock().unwrap().clone();
    if last.as_deref() != Some(projected_str.as_str()) {
        *state.last_cwd_note.lock().unwrap() = Some(projected_str.clone());
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
            path: projected_str,
        })));
    }
    // Mid-run appends parked their persistence (the run owned the session);
    // persist before the authoritative sync so the rebuilt mirror keeps them.
    let parked = std::mem::take(&mut *state.pending_ui_notes.lock().unwrap());
    for record in parked {
        let _ = persist_ui_note(session, state, notice_tx, &record).await;
    }
    let parked_journal = std::mem::take(&mut *state.pending_journal.lock().unwrap());
    if !parked_journal.is_empty() {
        // K4: the settle drain runs the same fail-loud discipline as every
        // typed-append face — bounded retries, then a durable loss record
        // and one facade notice. The turn has already converged here, so
        // there is nothing left to cancel; the record parks for the idle
        // drain when the storage is still down (it lands after recovery).
        let appender = session.journal_appender();
        let mut loss_notified = false;
        for (kind, payload) in parked_journal {
            if let Err(err) = append_typed_resilient(&appender, &kind, payload).await {
                if let Some(row) = record_journal_loss(&appender, &kind, &err).await {
                    state.pending_journal.lock().unwrap().push(row);
                }
                if kind != "error" && !loss_notified {
                    loss_notified = true;
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                        anyhow::anyhow!(
                            "journal append permanently failed for `{kind}` at settle: {err:#}; the entry was dropped"
                        ),
                    ))));
                }
            }
        }
    }
    sync_history(session, sessions_dir, state).await;
    sync_usage(session, state).await;
    spawn_session_list_refresh(sessions_dir, state);
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
    handle: &manox_harness::harness::HarnessHandle,
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCmd>,
    run_steers: &mut Vec<String>,
    shutdown_after_run: &mut bool,
    live: Arc<Mutex<LiveTranscript>>,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    pi_model: &mut PiModel,
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
        let journal_appender = session.journal_appender();
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
            &journal_appender,
        )
        .await;
        settle_run(
            &result,
            abort_requested,
            session,
            state,
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
) -> manox_harness::agent::Subscription {
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
) -> manox_harness::harness::HarnessSubscription {
    let tx = notice_tx.clone();
    let wake = wakeup_tx.clone();
    session.subscribe_harness(Arc::new(move |event| match event {
        // Idle-wakeup signal: a monitor steered events into the queue. The
        // actor decides whether the session is idle and resumes it
        // (`continue_` drains the steering queue first) — the listener stays
        // stateless and never touches the session itself.
        manox_harness::harness::HarnessEvent::QueueUpdate { steer, .. } if steer > 0 => {
            let _ = wake.send(());
        }
        manox_harness::harness::HarnessEvent::CompactionStart { .. } => {
            let _ = tx.send(BackendNotice::Event(Box::new(
                ThreadEvent::CompactionStarted { tokens_before: 0 },
            )));
        }
        manox_harness::harness::HarnessEvent::CompactionEnd {
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
            let retained_tail = adapt::harness_messages_to_messages(&result.retained_tail);
            let _ = tx.send(BackendNotice::Event(Box::new(ThreadEvent::Compaction {
                summary: result.summary,
                messages_compacted: 0,
                tokens_before: result.tokens_before,
                retained_tail,
            })));
        }
        _ => {}
    }))
}

/// Session header metadata: the creating host's identity, the owning thread
/// id (the retired worktree fork copied it verbatim, so historical fork
/// files still group under one thread), plus (for a team worker) the
/// leader's session id. The links persist with the jsonl
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
    granted_roots: &crate::granted_roots::GrantedRoots,
    thread_id: &str,
    parent_session: Option<&str>,
    bus: &Arc<crate::steer_bus::AgentBus>,
) -> (
    manox_harness::coding_agent::AgentSessionBuilder,
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
        granted_roots,
        bus,
    );
    let default_active = default_active_tool_names(&tools);
    let mut builder = create_agent_session()
        .with_cwd(cwd.to_path_buf())
        .with_session_dir(sessions_dir.to_path_buf())
        .with_model_runtime(runtime.clone())
        .with_system_prompt_builder(manox_harness::prompt::captain_prompt_builder(
            manox_harness::prompt::CaptainConfig {
                cwd: cwd.to_path_buf(),
                today: chrono::Local::now().format("%Y-%m-%d").to_string(),
                skills: crate::skill::summaries_or_empty()
                    .into_iter()
                    .map(|s| manox_harness::prompt::SkillSummary {
                        name: s.name,
                        description: s.description,
                    })
                    .collect(),
                lsp_ready_specs: {
                    // Format available LSP server ids as a comma-separated
                    // list for the system prompt's LSP ready line. Empty
                    // when no servers are available (the template omits the
                    // line entirely).
                    lsp::registry::try_global()
                        .map(|reg| {
                            let ids: Vec<&str> =
                                reg.available_specs().iter().map(|s| s.id).collect();
                            ids.join(", ")
                        })
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default()
                },
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
        manox_harness::harness::HookPoint::BeforeAgentStart,
        crate::plan_mode::injection_handler(Arc::clone(plan)),
    );
    let plans_dir = crate::paths::plans_dir().unwrap_or_else(|_| PathBuf::from(".manox/plans"));
    session.on(
        manox_harness::harness::HookPoint::ToolCall,
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
/// empty here — manox skills ride the `manox_agent::skill` registry instead.
fn instruction_resources(cwd: &Path) -> manox_harness::harness::HarnessResources {
    let set = crate::claude_md::load(cwd, &crate::settings::claude_md_load_context());
    let context_files = set
        .eager
        .iter()
        .map(|src| manox_harness::harness::ContextFile {
            name: src
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "CLAUDE.md".to_string()),
            location: src.path.display().to_string(),
            content: src.content.clone(),
        })
        .collect();
    manox_harness::harness::HarnessResources {
        skills: Vec::new(),
        prompt_templates: Vec::new(),
        context_files,
    }
}

/// Register the plugin-lifecycle hook bridges (PreToolUse / PostToolUse
/// fire-and-forget shell-outs). Notification-only; never blocks a call.
fn attach_plugin_hooks(session: &mut AgentSession, cwd: &Path) {
    session.on(
        manox_harness::harness::HookPoint::ToolCall,
        crate::plugin_hooks::pre_tool_call_handler(cwd.to_path_buf()),
    );
    session.on(
        manox_harness::harness::HookPoint::ToolResult,
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
    crate::provider_glue::wait_ready().await;
    // Bound the LSP registry probe wait: a missing/slow probe must never
    // stall session assembly (tools register without LSP when it misses).
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::lsp_tools::wait_ready(),
    )
    .await;
    let registry = crate::provider_glue::global();
    let runtime = ModelRuntime::with_provider_registry(registry.clone()).with_catalog(Arc::new(
        crate::provider_glue::LegacyAliasCatalog::new(registry.clone()),
    ));
    // Goal tools emit `GoalChanged` through the notice channel once the
    // actor owns it (facade-side operations emit on the gpui thread).
    if let Some(bridge) = &state.goal_bridge {
        bridge.set_sender(notice_tx.clone());
    }
    let Some(mut pi_model) = model.or_else(crate::provider_glue::default_model) else {
        let _ = notice_tx.send(BackendNotice::Fatal(anyhow::anyhow!(
            "no model configured — add a provider in Settings"
        )));
        return;
    };

    // Restore the requested session, else the newest one, else start fresh.
    // Tool cwd follows the restored session's project dir (the builder's
    // `open` re-pins cwd too).
    let repo = manox_harness::session::repository::SessionRepository::new(&sessions_dir);
    // `fresh` threads (sidebar new-conversation, project-bound creation)
    // never inherit the previous session; startup and explicit opens do.
    let latest = if fresh {
        None
    } else if let Some(requested) = &initial_path {
        // An explicit open reads only the requested transcript. The
        // store-wide `repo.list()` walk reads and parses every session
        // file — on a daily-use store that parked every thread switch
        // behind a full-store scan (#765 symptom: clicking a sidebar
        // thread never loads). The host filter is fail-closed as before.
        repo.info(requested)
            .await
            .ok()
            .filter(|info| crate::host::belongs_to_current_host(info.metadata.as_ref()))
    } else {
        repo.list().await.ok().and_then(|list| {
            // Only this host's sessions are eligible for restore; an explicit
            // path from another host is not restored (fail-closed).
            let mut list = list
                .into_iter()
                .filter(|info| crate::host::belongs_to_current_host(info.metadata.as_ref()));
            list.find(|info| info.message_count > 0)
        })
    };
    let mut restored = false;
    let mut session = None;
    if let Some(info) = latest {
        // Sessions created by a GUI launch (process cwd `/`) persisted a
        // useless cwd; heal them to this launch's default instead.
        let mut tool_cwd = PathBuf::from(info.cwd.clone());
        if tool_cwd.as_os_str() == "/" {
            tool_cwd = cwd.clone();
        }
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
            &state.granted_roots,
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
                &state.granted_roots,
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
    // The journal relay for this session (exits by itself when a swap drops
    // the storage; the next establishment spawns its own relay).
    spawn_journal_relay(&session, &state.journal_tx);
    // K5: publish the acceptance-time writer for this session.
    *state.current_appender.lock().unwrap() = Some(session.journal_appender());
    *state.current_resources.lock().unwrap() = Some(session.resources().clone());
    *state.active_path.lock().unwrap() = Some(session.path().to_path_buf());
    if let Some(project) = &project {
        bind_project(&sessions_dir, &session, project, &state, &notice_tx).await;
    }
    spawn_session_list_refresh(&sessions_dir, &state);

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
    // K2: the journal-first restore rebuild — the active chain is the
    // authority for every field §C.2 entries carry, the sidecar fills the
    // fields the chain has never seen, and a diverging cache is repaired
    // toward the journal.
    let restored_state = rebuild_restored_state(&session, &sessions_dir).await;
    let title_scheduler = TitleScheduler::new(
        runtime.clone(),
        Arc::clone(&state.model),
        Arc::clone(&live_mirror),
        state.goal_bridge.clone(),
        Arc::clone(&state.plan),
        session.path().to_path_buf(),
        cwd.clone(),
        cmd_tx.clone(),
        load_title_scheduler(
            &sessions_dir,
            session.path(),
            restored_provider_responses,
            restored_state.title.clone(),
        )
        .await,
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

    // The permission mode rebuilds from the journal (the sidecar fills the
    // legacy gap): restore it so a reopened session keeps its mode.
    let permission_mode = restored_state.permission_mode;
    state.gate.set_mode(permission_mode);
    // The reasoning effort rebuilds the same way: a reopened Max session
    // keeps its effort. The engine clamps against the current model and
    // persists the change in the transcript (TS `setThinkingLevel`).
    let reasoning_effort = restored_state.reasoning_effort;
    if reasoning_effort != ReasoningEffort::default()
        && let Err(err) = session
            .set_thinking_level(Some(reasoning_effort.wire_value().to_string()))
            .await
    {
        tracing::warn!(error = %err, "failed to restore reasoning effort");
    }
    // Plan mode rebuilds the same way: a reopened planning session keeps
    // its read-only gate; the facade re-renders and re-sends the
    // instructions once it sees `Ready`.
    state
        .plan
        .set(restored_state.plan_mode, restored_state.plan_file.clone());
    if restored_state.plan_mode {
        state
            .plan
            .set_active_instructions(render_plan_instructions());
    }
    let plan_review_pending = restored_state.plan_review_pending;
    let plan_snapshot = restored_state.plan_snapshot.clone();
    let restored_title = restored_state.title.clone();

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
        plan_mode: restored_state.plan_mode,
        plan_file: restored_state.plan_file.clone(),
        plan_review_pending,
        plan_snapshot,
        title: restored_title,
        goal: restored_state.goal.clone(),
        pinned: restored_state.pinned,
        archived: restored_state.archived,
        project: restored_state.project.clone(),
    })));
    // A restored session already "started": arm the SessionStart hook latch
    // so the first prompt does not re-fire it.
    if restored {
        state.session_start_fired.store(true, Ordering::SeqCst);
    }
    // Seed the cwd display right after `Ready`: a resumed session may
    // project an effective cwd (a `cwd_change` tail) that differs from the
    // launch directory, and the facade mirror starts empty. Seeding here
    // (and only here — settles handle the steady state) makes the first
    // `thread_info` already carry the directory the session works in.
    let projected = session.projected_cwd().await;
    let projected_str = projected.to_string_lossy().into_owned();
    *state.last_cwd_note.lock().unwrap() = Some(projected_str.clone());
    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
        path: projected_str,
    })));
    let mut run_steers: Vec<String> = Vec::new();
    let mut shutdown_after_run = false;

    loop {
        // Mid-run appends parked their persistence (the run owned the
        // session); drain before blocking on the next command.
        let parked = std::mem::take(&mut *state.pending_ui_notes.lock().unwrap());
        for record in parked {
            let _ = persist_ui_note(&session, &state, &notice_tx, &record).await;
        }
        let parked_journal = std::mem::take(&mut *state.pending_journal.lock().unwrap());
        if !parked_journal.is_empty() {
            // K4: same fail-loud discipline as the settle drain — bounded
            // retries, then a durable loss record and one facade notice. No
            // turn is in flight while idle, so there is nothing to cancel; a
            // still-down storage re-parks the record for the next drain.
            let appender = session.journal_appender();
            let mut loss_notified = false;
            for (kind, payload) in parked_journal {
                if let Err(err) = append_typed_resilient(&appender, &kind, payload).await {
                    if let Some(row) = record_journal_loss(&appender, &kind, &err).await {
                        state.pending_journal.lock().unwrap().push(row);
                    }
                    if kind != "error" && !loss_notified {
                        loss_notified = true;
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::Error(anyhow::anyhow!(
                                "journal append permanently failed for `{kind}`: {err:#}; the entry was dropped"
                            )),
                        )));
                    }
                }
            }
        }
        // A mid-run browser-suite toggle parked itself for the same reason;
        // apply it now that the session is idle again. The guard is dropped
        // before the await so the future stays `Send`.
        let parked_suite = state.pending_browser_suite.lock().unwrap().take();
        if let Some((suite, enable)) = parked_suite {
            apply_browser_suite(&mut session, suite, enable).await;
        }
        // Mid-run non-serviceable cmds (NewSession/Open/...) parked
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
                    let journal_appender = session.journal_appender();
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
                        &journal_appender,
                    )
                    .await;
                    settle_run(
                        &result,
                        abort_requested,
                        &session,
                        &state,
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
            SessionCmd::Prompt {
                text,
                images,
                origin_rpc,
                accepted_entry,
            } => {
                // K5: the prompt's user entry is on disk before the run
                // starts — persisted at Submit acceptance (the gateway
                // awaited the append before its receipt and passes the
                // entry id) or here, at drain, for a queued Submit. The
                // middleware skips its own append through the pinned id.
                if let Err(err) =
                    persist_prompt_user_entry(&session, &text, &images, origin_rpc, accepted_entry)
                        .await
                {
                    // Fail loud (the K4/K5 discipline): text that could not
                    // be persisted must not run — accepted-without-logged
                    // breaks the journal contract. The facade converges the
                    // turn it optimistically started.
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                        anyhow::anyhow!(
                            "submit persistence failed: {err:#}; the turn was not started"
                        ),
                    ))));
                    let _ = notice_tx.send(BackendNotice::Settled {
                        cancelled: false,
                        failed: true,
                        steered: Vec::new(),
                        stranded: Vec::new(),
                    });
                    continue;
                }
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
                let journal_appender = session.journal_appender();
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
                    &journal_appender,
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
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
                //
                // Every mirror follows the harness, never the request: a
                // refused switch (credential preflight, non-idle phase)
                // leaves the actor on the model the session still runs, and
                // the model already in play never appends a second
                // `model_change` entry.
                if pi_model != new_model {
                    if let Err(err) = session.set_model(new_model.clone()).await {
                        tracing::warn!("pi set_model failed: {err}");
                    } else {
                        // Keep the actor's working model in sync: Open/NewSession
                        // below build sessions with it.
                        pi_model = new_model.clone();
                        *state.model.lock().unwrap() = Some(new_model);
                    }
                }
            }
            SessionCmd::SetPermissionMode(mode) => {
                apply_permission_mode(&state, &sessions_dir, session.path(), mode, &notice_tx)
                    .await;
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
                // C4 vocabulary augmentation: the review edge rides the
                // journal (the pending projection's fold source — replay
                // and the P face; the sidecar flag demotes to the
                // pre-vocabulary hole-fill). "proposed"/"resolved" is the
                // fold vocabulary; the verdict discriminant rides the
                // notice plane.
                let review_state = if pending { "proposed" } else { "resolved" };
                if let Err(err) = session
                    .append_typed(
                        "plan_review",
                        serde_json::json!({
                            "state": review_state,
                            "planFile": state.plan.plan_file(),
                        }),
                    )
                    .await
                {
                    tracing::error!(%err, "failed to journal the plan review edge");
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
                    persist_initial_title(&sessions_dir, session.path(), title, &notice_tx).await;
                }
            }
            SessionCmd::PersistGeneratedTitle {
                session_path,
                title,
            } => {
                if session.path() == &session_path {
                    if let Err(error) =
                        persist_title(&sessions_dir, &session_path, title.clone()).await
                    {
                        tracing::warn!(%error, "failed to persist Title agent result");
                    } else {
                        let _ = notice_tx.send(BackendNotice::SessionListDirty);
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::TitleChanged { title },
                        )));
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
                            spawn_session_list_refresh(&sessions_dir, &state);
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
                let journal_appender = session.journal_appender();
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
                    &journal_appender,
                )
                .await;
                settle_run(
                    &result,
                    abort_requested,
                    &session,
                    &state,
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
                        spawn_session_list_refresh(&sessions_dir, &state);
                    }
                    Err(err)
                        if err
                            .downcast_ref::<manox_harness::compaction::NothingToCompact>()
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
                    &state.granted_roots,
                    &thread_id,
                    &bus,
                )
                .await;
                // K2: the journal-first rebuild resolves the swapped-in
                // session's observable state (authority: its active chain;
                // gap-fill: its sidecar).
                let restored_state = rebuild_restored_state(&session, &sessions_dir).await;
                title_scheduler.retarget(
                    path.clone(),
                    cwd.clone(),
                    load_title_scheduler(
                        &sessions_dir,
                        &path,
                        successful_provider_responses(session.harness_messages()),
                        restored_state.title.clone(),
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
                resync_plan_state(&restored_state, &state.plan, &notice_tx);
                // The opened file becomes the thread's active session.
                let opened_id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                crate::thread_registry::set_active(&thread_id, opened_id).await;
                *state.active_path.lock().unwrap() = Some(path);
                resync_approval_mode(&restored_state, &state, &notice_tx);
                // Opened sessions are resumed conversations: SessionStart
                // already happened in a prior lifetime.
                state.session_start_fired.store(true, Ordering::SeqCst);
                sync_history(&session, &sessions_dir, &state).await;
                sync_usage(&session, &state).await;
                spawn_session_list_refresh(&sessions_dir, &state);
            }
            SessionCmd::SetCwd { path } => {
                if !path.is_dir() {
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                        anyhow::anyhow!(
                            "set_cwd: working directory does not exist: {}",
                            path.display()
                        ),
                    ))));
                    continue;
                }
                // A no-op switch (the projected cwd already matches) only
                // refreshes the note — no duplicate `cwd_change` entry.
                if session.projected_cwd().await == path {
                    let path_str = path.to_string_lossy().into_owned();
                    *state.last_cwd_note.lock().unwrap() = Some(path_str.clone());
                    let _ =
                        notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
                            path: path_str,
                        })));
                    continue;
                }
                if let Err(err) = session.set_session_cwd(path.clone()).await {
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                        anyhow::anyhow!("set_cwd failed: {err:#}"),
                    ))));
                    continue;
                }
                let path_str = path.to_string_lossy().into_owned();
                *state.last_cwd_note.lock().unwrap() = Some(path_str.clone());
                let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::CwdChanged {
                    path: path_str,
                })));
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
                    &state.granted_roots,
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
                        session = s;
                        spawn_journal_relay(&session, &state.journal_tx);
                        // K5: the swapped session is the acceptance-time
                        // writer from here on.
                        *state.current_appender.lock().unwrap() = Some(session.journal_appender());
                        *state.current_resources.lock().unwrap() =
                            Some(session.resources().clone());
                        // The fresh session is pinned to the facade thread's id.
                        crate::thread_registry::set_active(&thread_id, &thread_id).await;
                        let new_path = session.path().to_path_buf();
                        // K2: a fresh chain folds empty — the rebuild
                        // resolves entirely to sidecar defaults, keeping
                        // one restore face for every session establishment.
                        let restored_state = rebuild_restored_state(&session, &sessions_dir).await;
                        title_scheduler.retarget(
                            new_path.clone(),
                            cwd.clone(),
                            load_title_scheduler(
                                &sessions_dir,
                                &new_path,
                                successful_provider_responses(session.harness_messages()),
                                restored_state.title.clone(),
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
                            bind_project(&sessions_dir, &session, project, &state, &notice_tx)
                                .await;
                        }
                        resync_approval_mode(&restored_state, &state, &notice_tx);
                        sync_history(&session, &sessions_dir, &state).await;
                        sync_usage(&session, &state).await;
                        spawn_session_list_refresh(&sessions_dir, &state);
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
                if persist_ui_note(&session, &state, &notice_tx, &record).await {
                    mirror_ui_note(&state, record);
                }
            }
            SessionCmd::AppendJournal { kind, payload } => {
                // Typed v4 journal entry from the notice tap: one durable
                // append per durable notice, in actor-queue order. K4: the
                // idle append runs the same fail-loud discipline — bounded
                // retries, then a durable loss record and a facade notice.
                // No turn is in flight while idle, so there is nothing to
                // cancel; a still-down storage parks the record for the next
                // drain.
                let appender = session.journal_appender();
                if let Err(err) = append_typed_resilient(&appender, &kind, payload).await {
                    if let Some(row) = record_journal_loss(&appender, &kind, &err).await {
                        state.pending_journal.lock().unwrap().push(row);
                    }
                    if kind != "error" {
                        let _ = notice_tx.send(BackendNotice::Event(Box::new(
                            ThreadEvent::Error(anyhow::anyhow!(
                                "journal append permanently failed for `{kind}`: {err:#}; the entry was dropped"
                            )),
                        )));
                    }
                }
            }
            SessionCmd::JournalSnapshot { reply } => {
                reply_journal_snapshot(&session.journal_appender(), reply).await;
            }
            SessionCmd::Shutdown => break,
        }
    }

    // K3 shutdown protocol: retire this thread's registry route and claim
    // every journal row still queued — a store-level pin/archive decision
    // races disposal (the gateway archives right after the last owner
    // detaches), and L3 leaves no observable state change without an
    // entry. Claimed rows append before close; the route itself is removed
    // only when this actor exits (the spawn wrapper), so a waiting store
    // dispatch cold-appends strictly after this storage stops writing.
    let claimed_rows = retire_and_claim_journal_rows(&thread_id, &mut cmd_rx);
    if !claimed_rows.is_empty() {
        let shutdown_appender = session.journal_appender();
        for (kind, payload) in claimed_rows {
            append_row_fail_loud(&shutdown_appender, kind, payload).await;
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
    granted_roots: &crate::granted_roots::GrantedRoots,
    thread_id: &str,
    bus: &Arc<crate::steer_bus::AgentBus>,
) {
    // The old session is replaced (its Drop runs on the actor thread); it is
    // already idle when a switch happens, so nothing in-flight is lost. The
    // project cwd comes from the opened transcript's own header — reading the
    // one file, never a store-wide `list` (the #765 thread-switch stall).
    let repo = manox_harness::session::repository::SessionRepository::new(sessions_dir);
    let cwd = repo
        .info(path)
        .await
        .ok()
        .map(|info| PathBuf::from(info.cwd))
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
        granted_roots,
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
            // Session swaps (Open) must carry the
            // same plugin lifecycle hooks as fresh builds, or the swapped
            // session's PreToolUse/PostToolUse fire-and-forget shell-outs
            // never attach (write confinement is now in ApprovalGatedTool).
            attach_plugin_hooks(&mut s, &cwd);
            adopt_session_model(&s, pi_model, state);
            *session = s;
            // The rebuilt session owns a new storage: its own journal relay.
            spawn_journal_relay(session, &state.journal_tx);
            // K5: the rebuilt session is the acceptance-time writer.
            *state.current_appender.lock().unwrap() = Some(session.journal_appender());
            *state.current_resources.lock().unwrap() = Some(session.resources().clone());
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
                    Some(
                        manox_harness::types::StopReason::Error
                            | manox_harness::types::StopReason::Aborted
                    )
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
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(
            |_model: &PiModel| -> Result<Arc<dyn manox_harness::agent_loop::StreamFn>, anyhow::Error> {
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
            result: manox_harness::tool::AgentToolResult {
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
    journal_title: Option<String>,
) -> PersistedTitleScheduler {
    // K2: the journal chain's title is the authority; the sidecar fills a
    // chain that never saw a `title` entry.
    let sidecar = manox_harness::session_meta::load(sessions_dir, session_path)
        .await
        .ok()
        .and_then(|meta| meta.title)
        .filter(|title| !title.trim().is_empty());
    PersistedTitleScheduler {
        title: journal_title.or(sidecar),
        provider_responses,
    }
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
                    Some(manox_harness::types::StopReason::Error | manox_harness::types::StopReason::Aborted)
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
    let current = manox_harness::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    if current.title.as_deref() == Some(title.as_str()) {
        return Ok(());
    }
    manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.title = Some(title.clone());
    })
    .await
}

/// The permission mode persisted in a session's sidecar (wire field
/// `approval_mode`); fresh sessions (missing sidecar or field) and unknown
/// values land on the bounded default. Test face of the sidecar cache —
/// production restores go through [`rebuild_restored_state`] (K2).
#[cfg(test)]
async fn load_approval_mode(sessions_dir: &Path, session_path: &Path) -> PermissionMode {
    match manox_harness::session_meta::load(sessions_dir, session_path).await {
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

/// Persist plan mode + last plan file from the shared state into the session
/// sidecar (`plan_mode` stored only while on; `plan_file` kept across exits
/// for the execution handoff).
async fn write_plan_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    plan: &crate::plan_mode::PlanSessionState,
) -> Result<(), anyhow::Error> {
    manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.plan_mode = plan.enabled().then_some(true);
        meta.plan_file = plan.plan_file();
    })
    .await
}

async fn write_plan_review_pending_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    pending: bool,
) -> Result<(), anyhow::Error> {
    manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.plan_review_pending = pending.then_some(true);
    })
    .await
}

async fn write_plan_snapshot_sidecar(
    sessions_dir: &Path,
    session_path: &Path,
    snapshot: Option<serde_json::Value>,
) -> Result<(), anyhow::Error> {
    manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.plan_snapshot = snapshot;
    })
    .await
}

/// Re-sync plan mode after a session switch (K2): the flag comes from the
/// journal-first restore rebuild. Emits `PlanModeChanged` so the facade
/// chip tracks the session it now mirrors; instructions re-render when the
/// target session plans.
fn resync_plan_state(
    restored: &RestoredThreadState,
    plan: &Arc<crate::plan_mode::PlanSessionState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    plan.set(restored.plan_mode, restored.plan_file.clone());
    plan.set_active_instructions(restored.plan_mode.then(render_plan_instructions).flatten());
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::PlanModeChanged {
            enabled: restored.plan_mode,
        },
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
    manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.approval_mode = Some(raw);
    })
    .await
}

/// The reasoning effort persisted in a session's sidecar; fresh sessions
/// (missing sidecar or field) default to High. Test face of the sidecar
/// cache — production restores go through [`rebuild_restored_state`] (K2).
#[cfg(test)]
async fn load_reasoning_effort(sessions_dir: &Path, session_path: &Path) -> ReasoningEffort {
    match manox_harness::session_meta::load(sessions_dir, session_path).await {
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
    manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.reasoning_effort = Some(effort.wire_value().to_string());
    })
    .await
}

/// Parse the facade's effort knob wire value; other levels (e.g. "off")
/// are not user-facing and yield `None`. Shared with the journal replay
/// fold (K2), which reads the same vocabulary off `thinking_level_change`
/// entries.
pub(crate) fn parse_reasoning_effort(raw: &str) -> Option<ReasoningEffort> {
    match raw {
        "high" => Some(ReasoningEffort::High),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

/// Re-apply the persisted permission mode after a session switch (K2): the
/// mode comes from the journal-first restore rebuild; align the gate and
/// the facade's chip with it.
fn resync_approval_mode(
    restored: &RestoredThreadState,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    state.gate.set_mode(restored.permission_mode);
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::PermissionModeChanged {
            mode: restored.permission_mode,
        },
    )));
}

/// The observable state of a restored thread (K2), resolved journal-first:
/// every field a §C.2 state-change entry carries rebuilds from the active
/// chain ([`crate::replay::replay_thread_state`]); the session sidecar is
/// the derived cache that fills fields the chain has never seen (legacy
/// journals predate the decision-point entries) and is repaired whenever
/// it diverges from a journal-backed field (conflicts resolve toward the
/// journal, never the reverse).
#[derive(Debug, Clone, PartialEq)]
struct RestoredThreadState {
    permission_mode: PermissionMode,
    reasoning_effort: ReasoningEffort,
    plan_mode: bool,
    /// Sidecar-only: the plan file has no journal vocabulary (the plan's
    /// content rides its own file; the entries carry mode + snapshot).
    plan_file: Option<String>,
    /// Journal-first (the C4 `plan_review` vocabulary): the chain's last
    /// review edge folds the pending flag; the sidecar hint is the
    /// pre-vocabulary hole-fill.
    plan_review_pending: bool,
    plan_snapshot: Option<serde_json::Value>,
    title: Option<String>,
    /// Journal authority (goal stage ②): the replayed last `goal` snapshot
    /// (`Some(Null)` = the explicit clear; `None` = the chain never saw one
    /// — the bridge keeps its db fold).
    goal: Option<serde_json::Value>,
    pinned: bool,
    archived: bool,
    project: Option<PathBuf>,
}

/// The replayed plan snapshot, with the model's cleared plan (an empty
/// array entry) normalized to the sidecar's absence semantics.
fn journal_plan_snapshot(
    replayed: &crate::replay::ReplayedThreadState,
) -> Option<serde_json::Value> {
    replayed
        .plan_snapshot
        .clone()
        .filter(|value| !value.as_array().is_some_and(|plan| plan.is_empty()))
}

/// Resolve the journal replay against the sidecar cache (the K2 merge):
/// journal-backed fields win wherever the chain carries them; the sidecar
/// fills exactly the fields the chain has never seen (legacy journals
/// predate the decision-point entries). Pure and total — every restore
/// face applies this resolution verbatim, and the replay-consistency
/// regression asserts it across a disk round trip.
fn merge_restored_state(
    replayed: &crate::replay::ReplayedThreadState,
    meta: &manox_harness::session_meta::SessionMeta,
) -> RestoredThreadState {
    let permission_mode = replayed.permission_mode.unwrap_or_else(|| {
        meta.approval_mode
            .as_deref()
            .and_then(PermissionMode::from_wire)
            .unwrap_or_default()
    });
    let reasoning_effort =
        replayed
            .reasoning_effort
            .unwrap_or_else(|| match meta.reasoning_effort.as_deref() {
                Some("max") => ReasoningEffort::Max,
                _ => ReasoningEffort::default(),
            });
    RestoredThreadState {
        permission_mode,
        reasoning_effort,
        plan_mode: replayed
            .plan_mode
            .unwrap_or(meta.plan_mode.unwrap_or(false)),
        plan_file: meta.plan_file.clone(),
        plan_review_pending: replayed
            .plan_review_pending
            .unwrap_or_else(|| meta.plan_review_pending.unwrap_or(false)),
        plan_snapshot: journal_plan_snapshot(replayed).or_else(|| meta.plan_snapshot.clone()),
        title: replayed
            .title
            .clone()
            .or_else(|| meta.title.clone().filter(|t| !t.trim().is_empty())),
        goal: replayed.goal.clone(),
        pinned: replayed.pinned.unwrap_or(meta.pinned),
        archived: replayed.archived.unwrap_or(meta.archived),
        project: match &replayed.project {
            Some(Some(path)) => Some(PathBuf::from(path)),
            Some(None) => None,
            None => meta.project.clone().map(PathBuf::from),
        },
    }
}

/// Fold one session's active chain and resolve it against the sidecar
/// cache (K2). Runs on every restore face: startup, `Open`, and
/// `NewSession` (a fresh chain resolves entirely to sidecar defaults).
async fn rebuild_restored_state(
    session: &AgentSession,
    sessions_dir: &Path,
) -> RestoredThreadState {
    let records = session.journal_range(0, u64::MAX).await.unwrap_or_default();
    let replayed = crate::replay::replay_thread_state(&records);
    let meta = manox_harness::session_meta::load(sessions_dir, session.path())
        .await
        .unwrap_or_default();
    let merged = merge_restored_state(&replayed, &meta);

    // Cache repair: re-stamp every journal-backed field whose sidecar copy
    // diverges from the authority, in one write. Fields the journal has
    // never seen keep their sidecar value untouched.
    let repair_mode = replayed
        .permission_mode
        .filter(|mode| meta.approval_mode.as_deref() != Some(mode.wire()));
    let repair_effort = replayed
        .reasoning_effort
        .filter(|effort| meta.reasoning_effort.as_deref() != Some(effort.wire_value()));
    let repair_plan_mode = replayed
        .plan_mode
        .filter(|enabled| meta.plan_mode.unwrap_or(false) != *enabled);
    let repair_snapshot =
        journal_plan_snapshot(&replayed).filter(|value| meta.plan_snapshot.as_ref() != Some(value));
    let repair_title = replayed
        .title
        .filter(|title| meta.title.as_deref() != Some(title.as_str()));
    // K2 title repair, ENABLED (the rename-route decision): the only
    // direct-sidecar title writer is `set_external_title`, which serves
    // EXTERNAL TUI sessions — they carry no pi journal chain, never reach
    // this rebuild, and their sidecar is desktop-authoritative by design
    // (the external process owns the session file, so a journaled rename
    // face does not apply and a cold journal append would race it). A pi
    // thread's sidecar title is written only by the auto-title scheduler,
    // whose decision journals the `title` entry first — so a divergence
    // here is a stale cache and the chain re-stamps it. A chain that never
    // saw a title keeps the sidecar value (hole-fill; the scheduler seeds
    // through `journal_title.or(sidecar)`). A future pi rename UI must
    // route a journaled face (`thread_store::rename_thread`,
    // pin_thread-isomorphic).
    let repair_flags = replayed
        .pinned
        .zip(replayed.archived)
        .filter(|(pin, arc)| meta.pinned != *pin || meta.archived != *arc);
    let repair_project = match &replayed.project {
        Some(Some(path)) if meta.project.as_deref() != Some(path.as_str()) => {
            Some(Some(path.clone()))
        }
        Some(None) if meta.project.is_some() => Some(None),
        _ => None,
    };
    if repair_mode.is_some()
        || repair_effort.is_some()
        || repair_plan_mode.is_some()
        || repair_snapshot.is_some()
        || repair_flags.is_some()
        || repair_project.is_some()
        || repair_title.is_some()
    {
        let result =
            manox_harness::session_meta::update(sessions_dir, session.path(), move |meta| {
                if let Some(mode) = repair_mode {
                    meta.approval_mode = Some(mode.wire().to_string());
                }
                if let Some(effort) = repair_effort {
                    meta.reasoning_effort = Some(effort.wire_value().to_string());
                }
                if let Some(enabled) = repair_plan_mode {
                    meta.plan_mode = enabled.then_some(true);
                }
                if let Some(snapshot) = repair_snapshot {
                    meta.plan_snapshot = Some(snapshot);
                }
                if let Some((pin, arc)) = repair_flags {
                    meta.pinned = pin;
                    meta.archived = arc;
                }
                if let Some(project) = repair_project {
                    meta.project = project;
                }
                if let Some(title) = repair_title {
                    meta.title = Some(title);
                }
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "failed to repair the session sidecar from the journal authority");
        }
    }

    merged
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
    manox_harness::session_meta::load(sessions_dir, session_path)
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
) -> std::collections::HashMap<usize, manox_harness::session_meta::UserAttributionMeta> {
    manox_harness::session_meta::load(sessions_dir, session_path)
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
    let meta = manox_harness::session_meta::load(sessions_dir, session_path)
        .await
        .unwrap_or_default();
    if meta.registry_displays.is_empty() && meta.user_attributions.is_empty() {
        return;
    }
    if let Err(err) = manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
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
        manox_harness::session_meta::UserAttributionMeta,
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
    if let Err(err) = manox_harness::session_meta::update(sessions_dir, session_path, |meta| {
        meta.project = Some(project.to_string_lossy().to_string());
    })
    .await
    {
        tracing::warn!(error = %err, "failed to persist session project");
    }
}

/// Bind a session to its project (K3): the sidecar cache write plus the
/// `project_change` journal entry — the entry is the authority (K2), the
/// sidecar stays the derived fast-list cache. The append runs the shared
/// fail-loud discipline; a still-down storage parks the loss record for
/// the next drain and the facade hears one `Error` notice.
async fn bind_project(
    sessions_dir: &Path,
    session: &AgentSession,
    project: &Path,
    state: &Arc<EngineState>,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    write_project_sidecar(sessions_dir, session.path(), project).await;
    let appender = session.journal_appender();
    let payload = serde_json::json!({ "path": project.to_string_lossy() });
    if let Err(err) = append_typed_resilient(&appender, "project_change", payload).await {
        if let Some(row) = record_journal_loss(&appender, "project_change", &err).await {
            state.pending_journal.lock().unwrap().push(row);
        }
        let _ = notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::Error(anyhow::anyhow!(
                "journal append permanently failed for `project_change`: {err:#}; the entry was dropped"
            )),
        )));
    }
}

/// Apply the user's permission-mode decision (K3): gate + sidecar cache +
/// the notice whose tap emission journals the `permission_mode_change`
/// entry (L3 — the decision never lives in the cache alone). The facade
/// already broadcast its own synchronous copy of this event; the echo is
/// idempotent.
async fn apply_permission_mode(
    state: &Arc<EngineState>,
    sessions_dir: &Path,
    session_path: &Path,
    mode: PermissionMode,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    state.gate.set_mode(mode);
    if let Err(err) = write_approval_mode_sidecar(sessions_dir, session_path, mode).await {
        tracing::warn!(error = %err, "failed to persist approval mode");
    }
    let _ = notice_tx.send(BackendNotice::Event(Box::new(
        ThreadEvent::PermissionModeChanged { mode },
    )));
}

/// Persist the initial-title decision (K3): the sidecar cache write, the
/// sidebar refresh, and the `TitleChanged` notice whose tap emission
/// journals the `title` entry (L3) — the same face the generated title
/// rides — while the facade mirror tracks the title bar without a reload.
async fn persist_initial_title(
    sessions_dir: &Path,
    session_path: &Path,
    title: String,
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
) {
    if let Err(error) = persist_title(sessions_dir, session_path, title.clone()).await {
        tracing::warn!(%error, "failed to persist initial title");
    } else {
        let _ = notice_tx.send(BackendNotice::SessionListDirty);
        let _ = notice_tx.send(BackendNotice::Event(Box::new(ThreadEvent::TitleChanged {
            title,
        })));
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
fn token_usage_from_totals(t: &manox_harness::coding_agent::usage::UsageTotals) -> TokenUsage {
    TokenUsage {
        input_tokens: t.input,
        output_tokens: t.output,
        cache_creation_input_tokens: t.cache_write,
        cache_read_input_tokens: t.cache_read,
    }
}

/// Map a pi usage report onto the manox token shape.
fn to_token_usage(u: &manox_harness::types::Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
    }
}

/// Mirror the session list into the actor's state as a detached scan.
///
/// `SessionRepository::list` reads and parses every transcript in the
/// store — an O(store) walk that takes seconds-to-minutes on a real
/// daily-use store. Awaiting it inline (the #765 regression) parked the
/// actor before its idle loop and at every settle point, so
/// `JournalSnapshot` (the follow-stream snapshot that carries the
/// projection baseline) and `SetModel` sat behind the scan and every
/// thread looked dead. The mirror is best-effort presentation state
/// (sidebar rows), so the walk runs on the side; the mutex swap is the
/// handoff.
fn spawn_session_list_refresh(sessions_dir: &Path, state: &Arc<EngineState>) {
    let sessions_dir = sessions_dir.to_path_buf();
    let state = Arc::clone(state);
    crate::runtime::handle().spawn(async move {
        refresh_session_list(&sessions_dir, &state).await;
    });
}

/// Re-read the session directory and mirror the summary list into engine
/// state (the sidebar's source of truth).
async fn refresh_session_list(sessions_dir: &Path, state: &Arc<EngineState>) {
    let repo = manox_harness::session::repository::SessionRepository::new(sessions_dir);
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
fn session_info_to_summary(
    info: &manox_harness::session::repository::SessionInfo,
) -> ThreadSummary {
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
/// and `manox_agent::Message` history (rebuild). A pi `AgentSession` produces
/// `AgentEvent`s and `AgentMessage`s; the functions here translate them into
/// those two shapes so the polished manox render pipeline is reused.
///
/// The harness<->pi translation is internal to the crate in production builds;
/// it is exposed as `pub` only under `test-support` so integration tests (e.g.
/// the agent-ui overlap walk) can rebuild a conversation from a captured
/// session without widening the production API surface.
#[cfg(feature = "test-support")]
#[path = "engine_adapt.rs"]
pub mod adapt;
#[cfg(not(feature = "test-support"))]
#[path = "engine_adapt.rs"]
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
        let meta = manox_harness::session_meta::SessionMeta {
            registry_displays: [
                (0usize, "/gitwork:deliver fast".to_string()),
                (2usize, "/healthz".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        manox_harness::session_meta::save(dir.path(), &session, &meta)
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
        let meta = manox_harness::session_meta::SessionMeta {
            user_attributions: [
                (
                    0usize,
                    manox_harness::session_meta::UserAttributionMeta {
                        author: "lead".into(),
                        peer: false,
                        display_text: None,
                    },
                ),
                (
                    1usize,
                    manox_harness::session_meta::UserAttributionMeta {
                        author: "Sailor".into(),
                        peer: true,
                        display_text: Some("unwrapped body".into()),
                    },
                ),
                // Beyond the transcript's ordinals: tolerated, attaches nowhere.
                (
                    5usize,
                    manox_harness::session_meta::UserAttributionMeta {
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
        manox_harness::session_meta::save(dir.path(), &session, &meta)
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
        let meta = manox_harness::session_meta::SessionMeta {
            registry_displays: [(0usize, "/gitwork:deliver fast".to_string())]
                .into_iter()
                .collect(),
            user_attributions: [(
                0usize,
                manox_harness::session_meta::UserAttributionMeta {
                    author: "lead".into(),
                    peer: false,
                    display_text: None,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        manox_harness::session_meta::save(dir.path(), &session, &meta)
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
        let meta = manox_harness::session_meta::SessionMeta {
            approval_mode: Some("yolo".to_string()),
            ..Default::default()
        };
        manox_harness::session_meta::save(dir.path(), &session, &meta)
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
        let meta = manox_harness::session_meta::SessionMeta {
            title: Some("my thread".to_string()),
            project: Some("/tmp/proj".to_string()),
            ..Default::default()
        };
        manox_harness::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();

        write_approval_mode_sidecar(dir.path(), &session, PermissionMode::DangerFullAccess)
            .await
            .unwrap();

        let loaded = manox_harness::session_meta::load(dir.path(), &session)
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
            vec![manox_harness::types::ContentBlock::Image {
                data: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
            }],
        );
        let manox_harness::types::AgentMessage::User { content, .. } = &msg else {
            panic!("steer message must be a user message");
        };
        assert_eq!(content.len(), 2, "text first, then the image block");
        assert!(matches!(
            &content[0],
            manox_harness::types::ContentBlock::Text { text, .. } if text == "look at this"
        ));
        assert!(matches!(
            &content[1],
            manox_harness::types::ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
        ));
    }

    #[test]
    fn agent_tool_start_maps_to_subagent_progress_row() {
        let events = adapt::agent_event_to_thread_events(
            &manox_harness::types::AgentEvent::ToolExecutionStart {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                arguments: serde_json::json!({
                    "subagent_type": "Explore",
                    "prompt": "find the auth module and summarize its structure",
                }),
            },
        );
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
        let events = adapt::agent_event_to_thread_events(
            &manox_harness::types::AgentEvent::ToolExecutionEnd {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                result: manox_harness::tool::AgentToolResult::text("done"),
                is_error: false,
            },
        );
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
        let events = adapt::agent_event_to_thread_events(
            &manox_harness::types::AgentEvent::ToolExecutionStart {
                tool_call_id: "call-2".into(),
                tool_name: "Read".into(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            },
        );
        assert_eq!(events.len(), 1, "plain tools keep a single tool card");
    }

    #[test]
    fn agent_child_text_delta_maps_to_subagent_child() {
        let events = adapt::agent_event_to_thread_events(
            &manox_harness::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-1".into(),
                tool_name: crate::tools::AGENT.into(),
                arguments: serde_json::json!({}),
                partial_result: serde_json::json!({
                    "subagent_event": { "kind": "text", "text": "found it" }
                }),
            },
        );
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
            &manox_harness::types::AgentEvent::ToolExecutionUpdate {
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
            &manox_harness::types::AgentEvent::ToolExecutionUpdate {
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
        let events = adapt::agent_event_to_thread_events(
            &manox_harness::types::AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-3".into(),
                tool_name: "Bash".into(),
                arguments: serde_json::json!({}),
                partial_result: serde_json::json!({ "output": "line one" }),
            },
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            crate::thread::ThreadEvent::ToolOutput { chunk, .. } if chunk == "line one"
        ));
    }
    #[test]
    fn adapt_strips_proposed_plan_blocks_from_assistant_text() {
        let plan = "## Steps\n- do the thing";
        let messages = vec![manox_harness::types::AgentMessage::Assistant {
            content: vec![manox_harness::types::ContentBlock::Text {
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
            stop_reason: Some(manox_harness::types::StopReason::Stop),
            raw_stop_reason: None,
            usage: Box::new(manox_harness::types::Usage::default()),
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

    /// L3机械化验证：每个被 tap 映射的 ThreadEvent，其 (kind, payload) 都
    /// 必须能构造出类型化 journal 条目——映射与词汇表永不脱钩。
    #[test]
    fn durable_journal_mapping_round_trips_every_journaled_event() {
        use crate::thread::{SubagentChildEvent, ThreadEvent, ToolCallStatus};
        let events: Vec<ThreadEvent> = vec![
            ThreadEvent::TurnStarted,
            ThreadEvent::TurnFinished {
                cancelled: false,
                failed: true,
                stranded_steer_ids: vec!["s-1".into()],
            },
            ThreadEvent::Stop(crate::language_model::StopReason::MaxTokens),
            ThreadEvent::Retry {
                attempt: 2,
                max_attempts: 5,
                delay_secs: 3,
                reason: "rate limited".into(),
                detail: Some("429".into()),
            },
            ThreadEvent::Error(anyhow::anyhow!("provider exploded")),
            ThreadEvent::AgentText("delta".into()),
            ThreadEvent::AgentThinking("think".into()),
            ThreadEvent::ToolCall {
                id: "tc-1".into(),
                name: "Bash".into(),
                title: "run ls".into(),
                status: ToolCallStatus::Running,
                input: Some(serde_json::json!({"command": "ls"})),
            },
            ThreadEvent::ToolResult {
                id: "tc-1".into(),
                output: "a b".into(),
                is_error: false,
            },
            ThreadEvent::ToolOutput {
                id: "tc-1".into(),
                chunk: "a".into(),
            },
            ThreadEvent::SubagentStarted {
                id: "sub-1".into(),
                subagent_type: "explore".into(),
                description: "scout".into(),
                child: crate::thread::ThreadId("child-1".into()),
            },
            ThreadEvent::SubagentProgress {
                id: "sub-1".into(),
                subagent_type: "explore".into(),
                tool_uses: 4,
                token_usage: Default::default(),
                latest_activity: Some("reading".into()),
                status: ToolCallStatus::Running,
                health: None,
            },
            ThreadEvent::SubagentChild {
                id: "sub-1".into(),
                child: SubagentChildEvent::Text("hi".into()),
            },
            ThreadEvent::PermissionModeChanged {
                mode: manox_harness::sandbox::PermissionMode::default(),
            },
            ThreadEvent::PlanModeChanged { enabled: true },
            ThreadEvent::PlanUpdated {
                snapshot: crate::plan::PlanSnapshot {
                    explanation: None,
                    steps: vec![],
                },
            },
            ThreadEvent::GoalChanged { goal: None },
            ThreadEvent::TitleChanged { title: "t".into() },
            ThreadEvent::BrowserSuitesChanged {
                suites: vec![crate::engine::BrowserSuite::ChromeUse],
            },
            // BackgroundTaskUpdated omitted: TaskSnapshot has no cheap test
            // constructor; its mapping is a mechanical to_value into a
            // JsonValue entry field covered by from_kind_payload tests.
            ThreadEvent::ToolCallAuthorization {
                id: "auth-1".into(),
                tool_name: "Bash".into(),
                summary: "run ls".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            ThreadEvent::CompactionStarted { tokens_before: 42 },
            ThreadEvent::TokenUsageUpdated(Default::default()),
            ThreadEvent::PrefixStability {
                stability_pct: 90,
                system_changed: false,
                tools_changed: false,
            },
            ThreadEvent::CacheInvalidation {
                reprocessed_tokens: 10,
            },
            ThreadEvent::SideCallMetricsUpdated(vec![]),
            ThreadEvent::MainCallMetricsUpdated(Default::default()),
        ];
        for event in &events {
            let (kind, payload) = durable_journal_payload(event)
                .unwrap_or_else(|| panic!("{event:?} must map to a journal kind"));
            let entry = manox_harness::session::SessionTreeEntry::from_kind_payload(
                &kind,
                "e-test".into(),
                None,
                chrono::Utc::now(),
                payload,
            )
            .unwrap_or_else(|err| panic!("kind {kind} payload must construct: {err}"));
            assert_eq!(entry.id(), "e-test");
        }
        // 已由各自归属流程持久化/快照语义的事件不得重复入日志。
        let excluded = vec![
            ThreadEvent::ModelChanged {
                from: None,
                to: "m".into(),
            },
            ThreadEvent::ReasoningEffortChanged {
                effort: crate::language_model::ReasoningEffort::High,
            },
            ThreadEvent::CwdChanged { path: "/p".into() },
            ThreadEvent::HistoryProgress,
            ThreadEvent::HistoryRestored,
        ];
        for event in &excluded {
            assert!(
                durable_journal_payload(event).is_none(),
                "{event:?} must not journal (owned by another flow)"
            );
        }
    }

    /// The journal relay maps storage appends (and Lagged) into the thread
    /// feed in order — the §C.3 host read face T4's follow streams ride.
    #[tokio::test]
    async fn journal_relay_feeds_storage_appends_in_seq_order() {
        crate::runtime::init();
        let dir = tempfile::tempdir().unwrap();
        let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(
            &dir.path().join("s.jsonl"),
            manox_harness::session::jsonl::JsonlSessionMetadata {
                id: "s1".into(),
                cwd: "/t".into(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: None,
            },
        )
        .await
        .unwrap();
        let state = test_engine_state();
        spawn_journal_relay_rx(storage.subscribe_journal(), state.journal_tx.clone());
        let mut feed = state.journal_tx.subscribe();

        use manox_harness::session::{SessionStorage, SessionTreeEntry};
        for i in 0..3 {
            let parent = if i == 0 {
                None
            } else {
                Some(format!("e{}", i - 1))
            };
            let parent = parent.as_deref();
            storage
                .append_entry(&SessionTreeEntry::TurnStart {
                    id: format!("e{i}"),
                    parent_id: parent.map(str::to_string),
                    timestamp: chrono::Utc::now(),
                })
                .await
                .unwrap();
        }
        for want in 0..3u64 {
            match feed.recv().await.expect("feed event") {
                JournalFeed::Event(ev) => assert_eq!(ev.seq, want),
                JournalFeed::Lagged(n) => panic!("unexpected lag {n}"),
            }
        }
    }

    fn test_engine_state() -> Arc<EngineState> {
        let cwd = std::env::temp_dir();
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
            pending_journal: Mutex::new(Vec::new()),
            journal_tx: tokio::sync::broadcast::channel(4096).0,
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
            current_appender: Mutex::new(None),
            current_resources: Mutex::new(None),
            gate,
            plan: crate::plan_mode::PlanSessionState::new(),
            goal_bridge: None,
            goal_continuation_reserved: AtomicBool::new(false),
            goal_continuation_round: Mutex::new(None),
            granted_roots: crate::granted_roots::GrantedRoots::new(cwd.clone()),
            last_cwd_note: Mutex::new(None),
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
            usage: Box::new(manox_harness::types::Usage::default()),
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
    impl manox_harness::agent_loop::StreamFn for MidRunModelStream {
        async fn stream(
            &self,
            context: &manox_harness::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
        ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
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
                    stop_reason: Some(manox_harness::types::StopReason::ToolUse),
                    usage: Box::new(manox_harness::types::Usage {
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
                    stop_reason: Some(manox_harness::types::StopReason::Stop),
                    usage: Box::new(manox_harness::types::Usage {
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
    impl manox_harness::tool::AgentTool for EchoTool {
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
            _ctx: &dyn manox_harness::tool::ToolContext,
        ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
            Ok(manox_harness::tool::AgentToolResult::text(
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
            thinking: manox_harness::types::ThinkingKind::None,
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
            thinking: manox_harness::types::ThinkingKind::None,
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
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let runtime = ModelRuntime::new(resolver);

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(runtime)
            .with_model(test_model())
            .with_tools(vec![
                Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
            ])
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
        let journal_appender = session.journal_appender();
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
            &journal_appender,
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
            // A re-pick of the model already in play changes nothing: the
            // second command must not append another entry.
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
        // The switch persisted as exactly one model_change entry, so a reload
        // attributes the same history the same way.
        let jsonl = tokio::fs::read_to_string(session.path()).await.unwrap();
        assert_eq!(
            jsonl.matches("\"modelId\":\"new\"").count(),
            1,
            "the mid-run switch must persist one model_change entry for the new model, even \
             when the same model is picked twice: {jsonl}"
        );
    }

    /// A stream that parks once mid-run (barrier), then answers with text —
    /// lets a test interleave a mid-run journal append while the turn is
    /// provably in flight.
    struct ParkOnceStream {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl manox_harness::agent_loop::StreamFn for ParkOnceStream {
        async fn stream(
            &self,
            context: &manox_harness::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
        ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
            self.started.notify_waiters();
            self.release.notified().await;
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
                stop_reason: Some(manox_harness::types::StopReason::Stop),
                usage: Box::new(manox_harness::types::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                }),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    /// A scripted provider that walks `rounds` rounds of TWO PARALLEL
    /// read-only tool calls before answering with plain text — the user's
    /// round-11 stall shape (three completed Grep rounds, then the fourth
    /// round's toolResult rows never landed and the turn spun forever).
    struct ToolRoundsStream {
        rounds: usize,
        call: std::sync::atomic::AtomicUsize,
    }

    fn tool_rounds_assistant(
        context: &manox_harness::types::AgentContext,
        round: usize,
    ) -> AgentMessage {
        let tool_use = |suffix: char| ContentBlock::ToolUse {
            id: format!("r{round}-{suffix}"),
            name: "Grep".into(),
            input: serde_json::json!({ "pattern": format!("needle{round}{suffix}") }),
            thought_signature: None,
        };
        AgentMessage::Assistant {
            content: vec![tool_use('a'), tool_use('b')],
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            api: context.model.api.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(manox_harness::types::StopReason::ToolUse),
            usage: Box::new(manox_harness::types::Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn text_assistant(context: &manox_harness::types::AgentContext, text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            model: context.model.id.clone(),
            provider: context.model.provider.clone(),
            api: context.model.api.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(manox_harness::types::StopReason::Stop),
            usage: Box::new(manox_harness::types::Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[async_trait::async_trait]
    impl manox_harness::agent_loop::StreamFn for ToolRoundsStream {
        async fn stream(
            &self,
            context: &manox_harness::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.rounds {
                Ok(tool_rounds_assistant(context, n))
            } else {
                Ok(text_assistant(context, "done"))
            }
        }
    }

    /// Round-11 regression: a multi-round turn of PARALLEL read-only tool
    /// calls must land EVERY round's durable rows — each round's two
    /// `tool_call` completions, their `tool_result` tap rows, AND the
    /// persisted `ToolResult` messages (the persistence middleware). The
    /// user's live stall: round 3's second Grep appended its success
    /// `tool_call` row and then nothing — no `tool_result` row, no ToolResult
    /// messages, turn frozen mid-flight forever. This test drives the real
    /// three-writer stack (persistence middleware + notice tap → live
    /// AppendJournal through drive_run's select) under a bounded timeout: a
    /// stall fails the timeout with a journal dump instead of hanging.
    #[tokio::test]
    async fn parallel_tool_rounds_land_every_result() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(ToolRoundsStream {
            rounds: 4,
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // The engine's notice wiring, replicated: session events → ThreadEvent
        // notices → the tap queues durable AppendJournal cmds onto the actor
        // channel; drive_run services them live (mid-run arm).
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
        let (tap_notice_tx, tap_notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let (facade_tx, _facade_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let _listener_sub = session.subscribe(Arc::new(move |event, _cancel| {
            let tx = tap_notice_tx.clone();
            Box::pin(async move {
                for te in crate::engine::adapt::agent_event_to_thread_events(&event) {
                    let _ = tx.send(BackendNotice::Event(Box::new(te)));
                }
            })
        }));
        let tap_cmd_tx = cmd_tx.clone();
        let _tap = tokio::spawn(async move {
            let mut rx = tap_notice_rx;
            while let Some(notice) = rx.recv().await {
                if let BackendNotice::Event(event) = &notice
                    && let Some((kind, payload)) = durable_journal_payload(event)
                {
                    let _ = tap_cmd_tx.send(SessionCmd::AppendJournal { kind, payload });
                }
            }
        });

        let state = test_engine_state();
        let live = Arc::new(Mutex::new(LiveTranscript::default()));
        let mut run_steers = Vec::new();
        let mut shutdown_after_run = false;
        let mut pi_model = test_model();
        let handle = session.handle();
        let sessions_path = dir.path().join("sessions");
        let active_session_path = session.path().clone();
        let journal_appender = session.journal_appender();

        let driven = drive_run(
            session.prompt("four rounds of parallel greps"),
            &handle,
            &mut cmd_rx,
            &mut run_steers,
            &mut shutdown_after_run,
            live,
            &state,
            &facade_tx,
            &mut pi_model,
            &sessions_path,
            &active_session_path,
            &journal_appender,
        );
        let appender_for_dump = std::sync::Arc::clone(&journal_appender);
        // Production pressure the basic flow lacks: the foreground leaf fires
        // GetConversationInfo on every committed-message change, and each one
        // routes through SessionCmd::JournalSnapshot into drive_run's LIVE
        // arm (round-9). A concurrent snapshot storm rides the whole turn.
        let snapshot_cmd_tx = cmd_tx.clone();
        let snapshot_storm = tokio::spawn(async move {
            for _ in 0..2000u32 {
                let (tx, rx) = tokio::sync::oneshot::channel::<JournalSnapshotData>();
                if snapshot_cmd_tx
                    .send(SessionCmd::JournalSnapshot { reply: tx })
                    .is_err()
                {
                    return;
                }
                let _ = rx.await;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });
        let (result, _aborted) =
            match tokio::time::timeout(std::time::Duration::from_secs(60), driven).await {
                Ok(pair) => pair,
                Err(_) => {
                    let rows = appender_for_dump
                        .storage()
                        .journal_range(0, u64::MAX)
                        .await
                        .unwrap_or_default();
                    let kinds: Vec<String> = rows
                        .iter()
                        .map(|r| match &r.entry {
                            manox_harness::session::SessionTreeEntry::ToolCall {
                                name,
                                status,
                                ..
                            } => {
                                format!("tool_call:{name}:{status}")
                            }
                            manox_harness::session::SessionTreeEntry::ToolResult { .. } => {
                                "tool_result".into()
                            }
                            manox_harness::session::SessionTreeEntry::Message { .. } => {
                                "message".into()
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        })
                        .collect();
                    panic!("drive_run stalled mid-turn; journal tail: {kinds:?}");
                }
            };
        result.unwrap();
        snapshot_storm.abort();

        // Post-run: the idle arm of the actor loop services any AppendJournal
        // cmds that raced past the run's completion (the tap task lags the
        // run by scheduler granularity).
        let appender_for_drain = std::sync::Arc::clone(&journal_appender);
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let SessionCmd::AppendJournal { kind, payload } = cmd
                && let Err(err) = appender_for_drain.append_typed(&kind, payload).await
            {
                tracing::warn!(%err, kind, "post-run journal append failed");
            }
        }

        let rows = journal_appender
            .storage()
            .journal_range(0, u64::MAX)
            .await
            .unwrap();
        // Every round's two tool calls must have BOTH their tap rows and their
        // persisted ToolResult messages.
        let tool_results = rows
            .iter()
            .filter(|r| {
                matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::ToolResult { .. }
                )
            })
            .count();
        let result_messages = rows
            .iter()
            .filter(|r| {
                matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::Message {
                        message: AgentMessage::ToolResult { .. },
                        ..
                    }
                )
            })
            .count();
        let kinds: Vec<String> = rows
            .iter()
            .map(|r| match &r.entry {
                manox_harness::session::SessionTreeEntry::ToolCall { name, status, .. } => {
                    format!("tool_call:{name}:{status:?}")
                }
                manox_harness::session::SessionTreeEntry::ToolResult { .. } => "tool_result".into(),
                manox_harness::session::SessionTreeEntry::Message { .. } => "message".to_string(),
                other => format!("{:?}", std::mem::discriminant(other)),
            })
            .collect();
        assert_eq!(
            tool_results, 8,
            "every parallel Grep must land a durable tool_result row; journal: {kinds:?}"
        );
        assert_eq!(
            result_messages, 8,
            "every parallel Grep must land its persisted ToolResult message; journal: {kinds:?}"
        );
    }

    /// A facade-level journal append (the notice tap: subagent progress,
    /// retries, background tasks) arriving while a turn is in flight must
    /// land in the journal IMMEDIATELY — not park for settle. The round-8
    /// repro: five dispatched Sailors failed fast and their rows sat in
    /// `pending_journal` while the Captain worked for 20+ minutes, so
    /// followers (follow streams, webui) saw nothing live.
    #[tokio::test]
    async fn mid_run_journal_append_lands_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(ParkOnceStream {
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
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
        let journal_appender = session.journal_appender();
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
            &journal_appender,
        );

        let appender_for_probe = std::sync::Arc::clone(&journal_appender);
        let ((result, _aborted), ()) = tokio::join!(run, async {
            // The turn is parked in flight; append a facade-level row.
            stream.started.notified().await;
            cmd_tx
                .send(SessionCmd::AppendJournal {
                    kind: "subagent_progress".into(),
                    payload: serde_json::json!({
                        "agentId": "review-kernel",
                        "agentType": "Sailor",
                        "toolUses": 0,
                        "tokenUsage": null,
                        "latestActivity": "failed: http 402",
                        "status": "error",
                    }),
                })
                .unwrap();
            // Bounded wait: the row must be readable from the shared journal
            // handle WHILE the run is still parked (nothing has settled).
            for _ in 0..10_000 {
                let rows = appender_for_probe
                    .storage()
                    .journal_range(0, u64::MAX)
                    .await
                    .unwrap_or_default();
                let found = rows.iter().any(|r| {
                    matches!(
                        &r.entry,
                        manox_harness::session::SessionTreeEntry::SubagentProgress {
                            agent_id, status, ..
                        }
                            if agent_id == "review-kernel"
                                && status.eq_ignore_ascii_case("error")
                    )
                });
                if found {
                    // Release the parked run from INSIDE the probe: the join
                    // below waits for the run too, so an outside release
                    // would deadlock.
                    stream.release.notify_waiters();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("the mid-run AppendJournal must land in the journal before settle");
        });
        result.unwrap();

        // Post-settle: the chain stays linear — the foreign row is part of
        // the active chain and the run's own rows append after it.
        let rows = session
            .journal_appender()
            .storage()
            .journal_range(0, u64::MAX)
            .await
            .unwrap();
        let foreign_ix = rows
            .iter()
            .position(|r| {
                matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::SubagentProgress { agent_id, .. }
                        if agent_id == "review-kernel"
                )
            })
            .expect("foreign row present after settle");
        // The run's own settled tail (an assistant Message row) appends
        // AFTER the foreign row: the chain stayed linear through the
        // mid-run interleaving.
        let done_ix = rows
            .iter()
            .rposition(|r| {
                matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::Message { .. }
                )
            })
            .expect("the run's settled message rows present");
        assert!(
            foreign_ix < done_ix,
            "the mid-run row precedes the run's settled tail (linear chain)"
        );
    }

    /// K4 probe stream: the first call answers with a parallel tool round
    /// (its assistant message materializes the deferred journal file), the
    /// next call parks until the test releases it — ignoring the
    /// cancellation signal like a provider stuck on the network.
    struct StallAfterFirstStream {
        call: std::sync::atomic::AtomicUsize,
        parked: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl manox_harness::agent_loop::StreamFn for StallAfterFirstStream {
        async fn stream(
            &self,
            context: &manox_harness::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                return Ok(tool_rounds_assistant(context, 0));
            }
            self.parked.notify_waiters();
            self.release.notified().await;
            Ok(text_assistant(context, "done"))
        }
    }

    /// K4 (P0) regression: a mid-run typed journal append that PERMANENTLY
    /// fails must not drop its row with a warn — the audited asymmetry let
    /// the state stand in effect while the journal silently lost its entry
    /// (L3); the control group (a message append failure) aborts the whole
    /// run. The fail-loud tail this pins: bounded retries, a durable `error`
    /// record of the loss (parked while the storage is down, landed after
    /// recovery), exactly ONE ThreadEvent::Error notice to the facade (the
    /// tap re-queues the notice itself as an `error` row — the kind guard
    /// must break that feedback loop, not storm), and a fail-closed turn
    /// K9 regression: a permanently failing UI-note append must fail LOUD
    /// like every typed-append face (K4 symmetry) — the durable loss record
    /// (parked for the settle/idle drains while the storage is down) and
    /// exactly one facade Error notice. Pre-fix a `tracing::warn` swallowed
    /// the loss: the plan/approval card vanished on reload with no journal
    /// trace.
    #[tokio::test]
    #[cfg(unix)]
    async fn ui_note_permanent_append_failure_fails_loud() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        // build() resolves the model stream eagerly; this probe never runs a
        // turn, so the stream fn itself is never called.
        struct K9IdleStream;
        #[async_trait::async_trait]
        impl manox_harness::agent_loop::StreamFn for K9IdleStream {
            async fn stream(
                &self,
                _context: &manox_harness::types::AgentContext,
                _signal: tokio_util::sync::CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
            ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
                Err(anyhow::anyhow!("the k9 probe never streams"))
            }
        }
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
            Ok(Arc::new(K9IdleStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();
        // Materialize the journal file (deferred until now): the durable
        // user append forces the header + row to disk.
        session
            .journal_appender()
            .append_message_durable(
                manox_harness::types::AgentMessage::User {
                    content: vec![manox_harness::types::ContentBlock::Text {
                        text: "k9 probe".into(),
                        signature: None,
                    }],
                    timestamp: chrono::Utc::now(),
                },
                None,
            )
            .await
            .unwrap();
        let journal_path = session.path().to_path_buf();
        // Fence the journal file: appends (open O_APPEND) fail with EACCES
        // while reads keep working.
        let original = std::fs::metadata(&journal_path)
            .unwrap()
            .permissions()
            .mode();
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o400)).unwrap();
        struct PermGuard(std::path::PathBuf, u32);
        impl Drop for PermGuard {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
            }
        }
        let guard = PermGuard(journal_path.clone(), original);
        if std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .is_ok()
        {
            // Root ignores the file mode — the fence is inert; skip rather
            // than assert on a false setup.
            return;
        }
        let state = test_engine_state();
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let landed = persist_ui_note(
            &session,
            &state,
            &notice_tx,
            &UiNoteRecord {
                kind: crate::db::UiNoteKind::Notice,
                data: serde_json::json!({ "text": "doomed card" }),
            },
        )
        .await;
        assert!(!landed, "the fenced append must not report success");
        // The loss record parks for the drains (storage down ⇒
        // record_journal_loss cannot land it either).
        let parked = state.pending_journal.lock().unwrap().clone();
        assert_eq!(parked.len(), 1, "exactly one parked loss record");
        assert_eq!(parked[0].0, "error");
        let message = parked[0].1["message"].as_str().unwrap();
        assert!(
            message.contains("journal append permanently failed for `ui_note`"),
            "the loss record names the dropped face: {message}"
        );
        // Exactly one facade notice.
        match notice_rx.recv().await {
            Some(BackendNotice::Event(event)) => match *event {
                ThreadEvent::Error(err) => assert!(
                    err.to_string().contains("ui_note"),
                    "the facade notice names the face: {err}"
                ),
                _ => panic!("expected the Error notice, got a different event"),
            },
            None => panic!("expected the facade Error notice, got a channel close"),
            Some(_) => panic!("expected an Event notice, got a Settled/other notice"),
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), notice_rx.recv())
                .await
                .is_err(),
            "the loss notice fires exactly once"
        );
        drop(guard);
    }

    /// cancel reported through drive_run's abort flag.
    #[tokio::test]
    #[cfg(unix)]
    async fn mid_run_typed_append_permanent_failure_fails_loud_and_cancels() {
        use std::os::unix::fs::PermissionsExt;

        // settle_run fires the (detached) plugin `Stop` hook through the
        // global runtime handle.
        crate::runtime::init();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(StallAfterFirstStream {
            call: std::sync::atomic::AtomicUsize::new(0),
            parked: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // Production notice wiring, replicated: every notice rides the tap,
        // which forwards to the facade FIRST and queues durable
        // AppendJournal cmds — including the `error` row for the serializer's
        // own fail-loud notice (the feedback loop the kind guard breaks).
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
        let (notice_tx, tap_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let (tap_tx, mut facade_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let tap_cmd_tx = cmd_tx.clone();
        let _tap = tokio::spawn(async move {
            let mut tap_rx = tap_rx;
            while let Some(notice) = tap_rx.recv().await {
                if let BackendNotice::Event(event) = &notice
                    && let Some((kind, payload)) = durable_journal_payload(event)
                {
                    let _ = tap_cmd_tx.send(SessionCmd::AppendJournal { kind, payload });
                }
                let _ = tap_tx.send(notice);
            }
        });
        let listener_tx = notice_tx.clone();
        let _listener_sub = session.subscribe(Arc::new(move |event, _cancel| {
            let tx = listener_tx.clone();
            Box::pin(async move {
                for te in crate::engine::adapt::agent_event_to_thread_events(&event) {
                    let _ = tx.send(BackendNotice::Event(Box::new(te)));
                }
            })
        }));

        let state = test_engine_state();
        let live = Arc::new(Mutex::new(LiveTranscript::default()));
        let mut run_steers = Vec::new();
        let mut shutdown_after_run = false;
        let mut pi_model = test_model();
        let handle = session.handle();
        let sessions_path = dir.path().join("sessions");
        let active_session_path = session.path().clone();
        let journal_appender = session.journal_appender();
        let journal_path = session.path().to_path_buf();

        let run = drive_run(
            session.prompt("k4 fail-loud probe"),
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
            &journal_appender,
        );

        let skipped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ((result, aborted), ()) = tokio::join!(run, async {
            // Turn 2 is parked in the provider stream; turn 1's assistant
            // message materialized the journal file on disk.
            stream.parked.notified().await;
            // Let the serializer drain turn 1's tap rows (quiesce) so the
            // probe row is the only append in flight when the fence rises.
            let mut last = usize::MAX;
            for _ in 0..1000 {
                let rows = journal_appender
                    .storage()
                    .journal_range(0, u64::MAX)
                    .await
                    .unwrap_or_default()
                    .len();
                if rows == last {
                    break;
                }
                last = rows;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            // Fence the journal file: appends (open O_APPEND) now fail with
            // EACCES while reads and the in-memory index keep working.
            let original = std::fs::metadata(&journal_path)
                .unwrap()
                .permissions()
                .mode();
            std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o400))
                .unwrap();
            // Restore the mode even on a panicking assert.
            struct PermGuard(std::path::PathBuf, u32);
            impl Drop for PermGuard {
                fn drop(&mut self) {
                    let _ =
                        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
                }
            }
            let guard = PermGuard(journal_path.clone(), original);
            // Root ignores the file mode; the fence would be inert, so skip
            // rather than assert on a false setup.
            if std::fs::OpenOptions::new()
                .append(true)
                .open(&journal_path)
                .is_ok()
            {
                skipped.store(true, Ordering::SeqCst);
                drop(guard);
                stream.release.notify_waiters();
                return;
            }
            cmd_tx
                .send(SessionCmd::AppendJournal {
                    kind: "subagent_progress".into(),
                    payload: serde_json::json!({
                        "agentId": "k4-loss-probe",
                        "agentType": "Sailor",
                        "toolUses": 0,
                        "tokenUsage": null,
                        "latestActivity": "working",
                        "status": "running",
                    }),
                })
                .unwrap();
            // The fail-loud notice must reach the facade WHILE the run is
            // still parked (bounded retries: 3 attempts, ~150ms of backoff).
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut saw_loss_notice = false;
            while tokio::time::Instant::now() < deadline && !saw_loss_notice {
                match tokio::time::timeout(std::time::Duration::from_millis(500), facade_rx.recv())
                    .await
                {
                    Ok(Some(BackendNotice::Event(ev))) => {
                        if let crate::thread::ThreadEvent::Error(err) = &*ev
                            && err.to_string().contains("subagent_progress")
                        {
                            saw_loss_notice = true;
                        }
                    }
                    Ok(Some(_)) => {}
                    // The facade channel closed: no notice can still arrive.
                    Ok(None) => break,
                    // A quiet 500ms window is not the deadline; keep waiting
                    // (the serializer's bounded retries plus scheduler load
                    // can stretch the notice past one window).
                    Err(_) => continue,
                }
            }
            assert!(
                saw_loss_notice,
                "a permanent mid-run append loss must notify the facade (K4 fail-loud)"
            );
            // Release the parked provider; the run converges (the serializer
            // already cancelled it fail-closed). The fence comes down right
            // after the release — on this runtime the woken stream cannot run
            // until this block yields — so the settle-side drains see a
            // healthy storage.
            stream.release.notify_waiters();
            drop(guard);
        });
        if skipped.load(Ordering::SeqCst) {
            eprintln!("skipping: running with write access despite 0o400 (root?)");
            return;
        }
        // (c) fail-closed convergence: the cancel reports through drive_run's
        // abort flag, so settle surfaces TurnFinished{cancelled}.
        assert!(
            aborted,
            "a permanent journal loss must cancel the turn fail-closed (K4)"
        );

        // Post-run: settle drains the parked loss record (the storage is
        // healthy again) — the durable `error` entry lands now, the
        // "visible after recovery" half of the K4 contract.
        settle_run(
            &result,
            aborted,
            &session,
            &state,
            &sessions_path,
            &cwd,
            &notice_tx,
            &mut run_steers,
        )
        .await;
        // Idle-arm replication for cmds that raced past the run (the tap's
        // `error` mirror of the fail-loud notice, late convergence rows).
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let SessionCmd::AppendJournal { kind, payload } = cmd
                && let Err(err) = append_typed_resilient(&journal_appender, &kind, payload).await
                && let Some(row) = record_journal_loss(&journal_appender, &kind, &err).await
            {
                state.pending_journal.lock().unwrap().push(row);
            }
        }

        // (a) the loss is recorded durably: an `error` entry on disk names
        // the dropped kind, and the dropped row itself never appears — the
        // journal never pretends the state change did not happen.
        let text = tokio::fs::read_to_string(&journal_path).await.unwrap();
        assert!(
            text.contains("journal append permanently failed for `subagent_progress`"),
            "the durable loss record must be on disk after recovery: {text}"
        );
        let rows = journal_appender
            .storage()
            .journal_range(0, u64::MAX)
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| matches!(
                &r.entry,
                manox_harness::session::SessionTreeEntry::SubagentProgress { agent_id, .. }
                    if agent_id == "k4-loss-probe"
            )),
            "the permanently lost row must not appear in the journal"
        );

        // (b) exactly one fail-loud notice: the tap re-queued the notice as
        // an `error` AppendJournal whose own fenced append must NOT notify
        // again (kind guard) — no feedback storm.
        let mut loss_notices = 1; // the one observed inside the probe block
        while let Ok(notice) = facade_rx.try_recv() {
            if let BackendNotice::Event(ev) = notice
                && let crate::thread::ThreadEvent::Error(err) = &*ev
                && err
                    .to_string()
                    .contains("journal append permanently failed")
            {
                loss_notices += 1;
            }
        }
        assert_eq!(
            loss_notices, 1,
            "the fail-loud notice fires once per run; the error-kind guard breaks the tap loop"
        );
    }

    /// The user-message rows of a journal read: `(joined text blocks,
    /// origin)` per entry — the K5 assertions' lens.
    fn k5_user_entries(
        rows: &[manox_harness::session::jsonl::JournalRecord],
    ) -> Vec<(String, Option<String>)> {
        rows.iter()
            .filter_map(|r| match &r.entry {
                manox_harness::session::SessionTreeEntry::Message {
                    message: AgentMessage::User { content, .. },
                    origin,
                    ..
                } => {
                    let text = content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    Some((text, origin.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Minimal drive_run wiring for the K5 tests: no tap (the assertions
    /// look at message entries only), a kept-alive cmd sender so the select
    /// never sees a closed channel, and a shared appender for the
    /// acceptance-side writes.
    struct K5Rig {
        cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
        _cmd_tx: mpsc::UnboundedSender<SessionCmd>,
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
        state: Arc<EngineState>,
        live: Arc<Mutex<LiveTranscript>>,
        run_steers: Vec<String>,
        shutdown_after_run: bool,
        pi_model: PiModel,
        handle: manox_harness::harness::HarnessHandle,
        sessions_path: PathBuf,
        active_session_path: PathBuf,
        journal_appender: Arc<JournalAppender>,
    }

    impl K5Rig {
        fn new(dir: &tempfile::TempDir, session: &AgentSession) -> Self {
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
            let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
            K5Rig {
                cmd_rx,
                _cmd_tx: cmd_tx,
                notice_tx,
                state: test_engine_state(),
                live: Arc::new(Mutex::new(LiveTranscript::default())),
                run_steers: Vec::new(),
                shutdown_after_run: false,
                pi_model: test_model(),
                handle: session.handle(),
                sessions_path: dir.path().join("sessions"),
                active_session_path: session.path().clone(),
                journal_appender: session.journal_appender(),
            }
        }
    }

    /// K5 (P0) regression, direct-submit path: a user entry persisted at
    /// Submit acceptance (durable — a deferred session materializes on it)
    /// is on disk BEFORE the run exists, the run's own user `MessageEnd`
    /// records the accepted entry instead of appending a duplicate, and the
    /// origin rides the accepted entry (echo retirement, §F.2).
    #[tokio::test]
    async fn accepted_user_entry_persists_before_the_run_and_the_middleware_skips_the_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(ToolRoundsStream {
            rounds: 0, // first call answers "done": the run completes
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // Submit acceptance (the gateway side, replicated): the durable
        // append materializes the still-deferred session — the accepted
        // text is on disk before any run exists.
        let appender = session.journal_appender();
        let accepted_id = appender
            .append_message_durable(
                prompt_user_message("k5 accepted text", &[]),
                Some("rpc-k5".into()),
            )
            .await
            .unwrap();
        assert!(
            session.path().exists(),
            "acceptance must put the entry on disk (a deferred session materializes)"
        );

        // The actor's Prompt-cmd resolution for an accepted Submit: arm the
        // middleware skip from the accepted entry id.
        let pinned = persist_prompt_user_entry(
            &session,
            "k5 accepted text",
            &[],
            Some("rpc-k5".into()),
            Some(accepted_id.clone()),
        )
        .await
        .unwrap();
        assert_eq!(pinned.as_deref(), Some(accepted_id.as_str()));

        let mut rig = K5Rig::new(&dir, &session);
        let (result, _aborted) = drive_run(
            session.prompt("k5 accepted text"),
            &rig.handle,
            &mut rig.cmd_rx,
            &mut rig.run_steers,
            &mut rig.shutdown_after_run,
            Arc::clone(&rig.live),
            &rig.state,
            &rig.notice_tx,
            &mut rig.pi_model,
            &rig.sessions_path,
            &rig.active_session_path,
            &rig.journal_appender,
        )
        .await;
        result.unwrap();

        let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
        let users = k5_user_entries(&rows);
        assert_eq!(
            users.len(),
            1,
            "the accepted entry is the ONLY user row (the middleware skipped its duplicate): {users:?}"
        );
        assert_eq!(users[0].0, "k5 accepted text");
        assert_eq!(
            users[0].1.as_deref(),
            Some("rpc-k5"),
            "the origin rides the accepted entry (echo retirement)"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::Message {
                        message: AgentMessage::Assistant { .. },
                        ..
                    }
                ))
                .count(),
            1,
            "the run completed and appended its assistant message"
        );
    }

    /// K5 regression, kill-after-receipt: while a turn is parked at a
    /// barrier, an accepted Submit's user entry is ALREADY in the journal
    /// (before the run continues); dropping everything right after the
    /// acceptance — the crash window the receipt opened — leaves the entry
    /// and its pinned origin on disk when the file is reopened.
    #[tokio::test]
    async fn kill_after_receipt_keeps_the_accepted_entry_and_origin() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(ParkOnceStream {
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // Acceptance: durable append + armed skip, exactly the direct-submit
        // mechanism.
        let appender = session.journal_appender();
        let accepted_id = appender
            .append_message_durable(prompt_user_message("k5b text", &[]), Some("rpc-k5b".into()))
            .await
            .unwrap();
        persist_prompt_user_entry(
            &session,
            "k5b text",
            &[],
            Some("rpc-k5b".into()),
            Some(accepted_id),
        )
        .await
        .unwrap();

        let journal_path = session.path().clone();
        let mut rig = K5Rig::new(&dir, &session);
        // The probe wins the select and the drive_run future is dropped
        // WITHOUT releasing the park: everything after the receipt dies
        // mid-turn (the crash the receipt's window invited).
        tokio::select! {
            _ = drive_run(
                session.prompt("k5b text"),
                &rig.handle,
                &mut rig.cmd_rx,
                &mut rig.run_steers,
                &mut rig.shutdown_after_run,
                Arc::clone(&rig.live),
                &rig.state,
                &rig.notice_tx,
                &mut rig.pi_model,
                &rig.sessions_path,
                &rig.active_session_path,
                &rig.journal_appender,
            ) => panic!("the parked run must not finish before the probe"),
            _ = async {
                stream.started.notified().await;
                // The turn is parked; the run never reached completion —
                // yet the accepted entry is already durable.
                let text = tokio::fs::read_to_string(&journal_path).await.unwrap();
                assert!(
                    text.contains("k5b text") && text.contains(r#""origin":"rpc-k5b""#),
                    "the accepted entry with its origin must be in the journal before the run continues: {text}"
                );
                assert_eq!(
                    text.matches("k5b text").count(),
                    1,
                    "the middleware skip kept the announced message from duplicating the entry: {text}"
                );
            } => {}
        }

        // Kill-after-receipt: everything is dropped (the select consumed the
        // run future); reopen the file — the entry and origin survived.
        let text = tokio::fs::read_to_string(&journal_path).await.unwrap();
        assert!(
            text.contains("k5b text") && text.contains(r#""origin":"rpc-k5b""#),
            "the accepted entry must survive the killed run: {text}"
        );
        let reopened = manox_harness::session::jsonl::JsonlSessionStorage::open(&journal_path)
            .await
            .unwrap();
        let rows = reopened.journal_range(0, u64::MAX).await.unwrap();
        let users = k5_user_entries(&rows);
        assert_eq!(users.len(), 1, "reopened journal: {users:?}");
        assert_eq!(users[0].0, "k5b text");
        assert_eq!(users[0].1.as_deref(), Some("rpc-k5b"));
    }

    /// K5 regression, queued-submit path: a Submit whose acceptance did not
    /// persist (the engine was running; the gateway queued it) persists at
    /// drain — in the actor's Prompt handler, BEFORE the run starts, so
    /// before model-visible — and the run's middleware skips the duplicate.
    #[tokio::test]
    async fn queued_submit_persists_at_drain_before_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(ToolRoundsStream {
            rounds: 0,
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // The actor's drain of a queued Submit: no accepted_entry, an
        // origin → persist NOW (durable), before drive_run exists.
        let drained_id =
            persist_prompt_user_entry(&session, "drained text", &[], Some("rpc-q".into()), None)
                .await
                .unwrap()
                .expect("an origin-carrying prompt persists at drain");

        let appender = session.journal_appender();
        let rows_before = appender.storage().journal_range(0, u64::MAX).await.unwrap();
        let users_before = k5_user_entries(&rows_before);
        assert_eq!(
            users_before,
            vec![("drained text".to_string(), Some("rpc-q".into()))],
            "the drained Submit must be in the journal BEFORE the run starts"
        );

        let mut rig = K5Rig::new(&dir, &session);
        let (result, _aborted) = drive_run(
            session.prompt("drained text"),
            &rig.handle,
            &mut rig.cmd_rx,
            &mut rig.run_steers,
            &mut rig.shutdown_after_run,
            Arc::clone(&rig.live),
            &rig.state,
            &rig.notice_tx,
            &mut rig.pi_model,
            &rig.sessions_path,
            &rig.active_session_path,
            &rig.journal_appender,
        )
        .await;
        result.unwrap();

        let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
        let users = k5_user_entries(&rows);
        assert_eq!(users.len(), 1, "no duplicate user row: {users:?}");
        assert_eq!(users[0].1.as_deref(), Some("rpc-q"));
        let ids: Vec<&str> = rows
            .iter()
            .filter(|r| {
                matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::Message {
                        message: AgentMessage::User { .. },
                        ..
                    }
                )
            })
            .map(|r| r.entry.id())
            .collect();
        assert_eq!(
            ids,
            vec![drained_id.as_str()],
            "the surviving user row IS the drain-time entry (the middleware recorded it, never re-appended)"
        );
    }

    /// K5 guard: a pin no run consumed (its run died before announcing the
    /// user message) must never leak the middleware skip into a later turn —
    /// even when the later turn's user content matches the stale pin
    /// exactly. The drain-time resolution clears the pin before arming (or
    /// declining to arm) its own.
    #[tokio::test]
    async fn stale_accepted_pin_never_leaks_into_the_next_turn() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let stream = Arc::new(ToolRoundsStream {
            rounds: 0,
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let stream_for_resolver = Arc::clone(&stream);
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // A stale pin whose content MATCHES the next turn's user message
        // (the worst case for a leaked skip): a dead run left it armed.
        let appender = session.journal_appender();
        let stale_content = match prompt_user_message("fresh text", &[]) {
            AgentMessage::User { content, .. } => serde_json::to_value(content).unwrap(),
            _ => unreachable!("prompt_user_message builds a user message"),
        };
        appender.pin_accepted_user_entry("stale-entry-id".into(), stale_content);

        // A legacy (origin-less) prompt: the drain-time resolution declines
        // to persist — and clears the stale pin on its way through.
        let pinned = persist_prompt_user_entry(&session, "fresh text", &[], None, None)
            .await
            .unwrap();
        assert!(
            pinned.is_none(),
            "a legacy prompt keeps the middleware path"
        );

        let mut rig = K5Rig::new(&dir, &session);
        let (result, _aborted) = drive_run(
            session.prompt("fresh text"),
            &rig.handle,
            &mut rig.cmd_rx,
            &mut rig.run_steers,
            &mut rig.shutdown_after_run,
            Arc::clone(&rig.live),
            &rig.state,
            &rig.notice_tx,
            &mut rig.pi_model,
            &rig.sessions_path,
            &rig.active_session_path,
            &rig.journal_appender,
        )
        .await;
        result.unwrap();

        let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
        let users = k5_user_entries(&rows);
        assert_eq!(users.len(), 1, "the legacy turn appended its own user row");
        assert_eq!(users[0].0, "fresh text");
        assert_eq!(users[0].1, None);
        let id = rows
            .iter()
            .find(|r| {
                matches!(
                    &r.entry,
                    manox_harness::session::SessionTreeEntry::Message {
                        message: AgentMessage::User { .. },
                        ..
                    }
                )
            })
            .map(|r| r.entry.id().to_string())
            .unwrap();
        assert_ne!(
            id, "stale-entry-id",
            "the surviving row is the middleware's fresh append, not the stale pin's id"
        );
    }

    /// `AppendUiNote` arriving while a turn is in flight must not be dropped
    /// like the other unserviceable mid-run commands: the card merges into
    /// the mirror immediately (a mid-run switch-back renders it) and parks
    /// its persistence for the idle loop, which appends the `custom` entry
    /// at the leaf once the run owns no borrow of the session.
    #[tokio::test]
    async fn mid_run_append_ui_note_mirrors_now_and_parks_persist() {
        // The settle path fires plugin hooks, and Registry::fire takes the
        // global runtime handle unconditionally — without this the test
        // only passes when an earlier test in the binary happened to init
        // the runtime (pre-existing isolation fragility, HEAD-verified:
        // alone it panics "tokio runtime not initialized").
        crate::runtime::init();
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
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream_for_resolver) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let runtime = ModelRuntime::new(resolver);

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(runtime)
            .with_model(test_model())
            .with_tools(vec![
                Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
            ])
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
        let journal_appender = session.journal_appender();
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
            &journal_appender,
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
        settle_run(
            &result,
            false,
            &session,
            &state,
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
    impl manox_harness::agent_loop::StreamFn for StaticStream {
        async fn stream(
            &self,
            context: &manox_harness::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<manox_harness::types::AgentEvent>,
        ) -> Result<manox_harness::types::AgentMessage, anyhow::Error> {
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
                stop_reason: Some(manox_harness::types::StopReason::Stop),
                usage: Box::new(manox_harness::types::Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    /// Restores the two test models from their session references.
    struct TestModelCatalog;

    impl manox_harness::coding_agent::model_runtime::ModelCatalog for TestModelCatalog {
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

        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
            Ok(Arc::new(StaticStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let runtime = ModelRuntime::new(resolver).with_catalog(Arc::new(TestModelCatalog));

        // Phase 1: a session that ran under `test` and switched to `new`.
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&sessions)
            .with_agent_dir(&agent)
            .with_model_runtime(runtime.clone())
            .with_model(test_model())
            .with_tools(vec![
                Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
            ])
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
            .with_tools(vec![
                Arc::new(EchoTool) as Arc<dyn manox_harness::tool::AgentTool>
            ])
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
        let meta = manox_harness::session_meta::SessionMeta {
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        manox_harness::session_meta::save(dir.path(), &session, &meta)
            .await
            .unwrap();
        assert_eq!(
            load_reasoning_effort(dir.path(), &session).await,
            ReasoningEffort::High,
            "unknown persisted efforts fall back to the default"
        );
    }

    fn assistant_usage(cache_read: u64) -> manox_harness::types::Usage {
        manox_harness::types::Usage {
            input_tokens: 10,
            cache_read_input_tokens: cache_read,
            ..Default::default()
        }
    }

    fn assistant_request(usage: manox_harness::types::Usage) -> AgentMessage {
        AgentMessage::Assistant {
            content: Vec::new(),
            model: "deepseek-v4-flash".into(),
            provider: "DeepSeek".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(manox_harness::types::StopReason::Stop),
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
        let mut registry = manox_harness::ext_point_agent::AgentRegistry::new();
        manox_harness::agents::register_defaults(&mut registry);
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
        let mut registry = manox_harness::ext_point_agent::AgentRegistry::new();
        manox_harness::agents::register_defaults(&mut registry);
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
    impl manox_harness::tool::AgentTool for MarkerBash {
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
            _: &dyn manox_harness::tool::ToolContext,
        ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError> {
            Ok(manox_harness::tool::AgentToolResult::text("foreground-ok"))
        }
    }

    #[tokio::test]
    async fn subagent_bash_refuses_background_but_allows_foreground() {
        use std::path::PathBuf;
        let tool = SubagentBashTool {
            inner: Arc::new(MarkerBash),
        };
        let ctx = manox_harness::tool::LocalToolContext::new(
            Arc::new(manox_harness::env::TokioExecutionEnv::new(PathBuf::from(
                "/tmp",
            ))),
            PathBuf::from("/tmp"),
            Arc::new(manox_harness::tool::ToolState::new()),
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
                if let manox_harness::types::ContentBlock::Text { text, .. } = b {
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

        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(|_m: &PiModel| {
            Ok(Arc::new(StaticStream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let runtime = ModelRuntime::new(resolver);

        let mut registry = AgentRegistry::new();
        register_defaults(&mut registry);
        let registry = Arc::new(registry);
        let tools: Vec<Arc<dyn manox_harness::tool::AgentTool>> = vec![
            Arc::new(manox_harness::tools::read::ReadTool),
            Arc::new(manox_harness::tools::grep::GrepTool),
            Arc::new(manox_harness::tools::glob::GlobTool),
            Arc::new(manox_harness::tools::ls::LsTool),
        ];
        let ctx = manox_harness::tool::LocalToolContext::new(
            Arc::new(manox_harness::env::TokioExecutionEnv::new(cwd.clone())),
            cwd.clone(),
            Arc::new(manox_harness::tool::ToolState::new()),
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

    // ── K3/K2/K1: decision-point entries, journal authority, replay gate ──

    /// One entry per replay-supported journal kind, in the payload shapes
    /// the production write faces emit (`durable_journal_payload` and the
    /// typed-append call sites). The K1 regression drives these through the
    /// real append face and asserts the coverage list below against the
    /// resulting chain.
    fn scripted_state_rows() -> Vec<(&'static str, serde_json::Value)> {
        use serde_json::json;
        vec![
            ("turn_start", json!({})),
            ("agent_text_delta", json!({ "delta": "hel" })),
            ("agent_thinking_delta", json!({ "delta": "thinking..." })),
            (
                "tool_call",
                json!({ "callId": "call-1", "name": "Read", "title": "Read a file", "status": "pending_approval", "input": { "path": "/tmp/x" } }),
            ),
            (
                "approval",
                json!({ "kind": "request", "authId": "call-1", "payload": { "toolName": "Read", "summary": "s", "input": {} } }),
            ),
            (
                "approval",
                json!({ "kind": "decision", "authId": "call-1", "payload": { "toolName": "Read", "verdict": "allow_once" } }),
            ),
            (
                "tool_result",
                json!({ "callId": "call-1", "output": "ok", "isError": false }),
            ),
            (
                "tool_output_chunk",
                json!({ "callId": "call-1", "chunk": "stream" }),
            ),
            (
                "subagent_child",
                json!({ "agentId": "agent-1", "event": { "type": "started", "subagentType": "Sailor", "description": "d", "childId": "c-1" } }),
            ),
            (
                "subagent_progress",
                json!({ "agentId": "agent-1", "agentType": "Sailor", "toolUses": 2, "tokenUsage": null, "latestActivity": "working", "status": "running" }),
            ),
            (
                "retry",
                json!({ "attempt": 1, "maxAttempts": 3, "delaySecs": 2, "reason": "overloaded", "detail": null }),
            ),
            (
                "model_change",
                json!({ "provider": "test", "modelId": "replay-model" }),
            ),
            ("cwd_change", json!({ "cwd": "/replay/work" })),
            ("project_change", json!({ "path": "/replay/proj" })),
            (
                "permission_mode_change",
                json!({ "mode": "danger-full-access" }),
            ),
            ("plan_mode_change", json!({ "enabled": true })),
            (
                "plan_update",
                json!({ "snapshot": [{ "content": "step one", "status": "pending", "activeForm": "stepping" }] }),
            ),
            (
                "plan_review",
                json!({ "state": "proposed", "planFile": "/replay/plan.md" }),
            ),
            ("goal", json!({ "goal": { "objective": "replay" } })),
            ("title", json!({ "title": "replayed title" })),
            ("browser_suites", json!({ "suites": ["chrome_use"] })),
            (
                "background_task",
                json!({ "snapshot": { "taskId": "task-1", "status": "running" } }),
            ),
            (
                "pinned_archived",
                json!({ "pinned": true, "archived": false }),
            ),
            (
                "pinned_archived",
                json!({ "pinned": false, "archived": true }),
            ),
            ("compaction_started", json!({ "tokensBefore": 1234 })),
            (
                "metrics",
                json!({ "metricType": "prefix_stability", "data": { "stabilityPct": 99, "systemChanged": false, "toolsChanged": false } }),
            ),
            ("stop", json!({ "reason": "end_turn" })),
            (
                "turn_finish",
                json!({ "cancelled": false, "failed": false, "strandedSteerIds": [] }),
            ),
            ("error", json!({ "message": "scripted error row" })),
        ]
    }

    /// K5 edge: the middleware pin (and the queued-submit drain
    /// persistence) carries the POST-expansion text — the shape the run
    /// announces — so a slash-command prompt's content-match skip holds
    /// and the journal never double-entries. The raw text must NOT match
    /// the pin.
    #[tokio::test]
    async fn prompt_pin_carries_the_expanded_text() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let stream = Arc::new(ToolRoundsStream {
            rounds: 0,
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let resources = manox_harness::harness::HarnessResources {
            prompt_templates: vec![manox_harness::harness::PromptTemplate {
                name: "deploy".into(),
                content: "deploy $ARGUMENTS now".into(),
            }],
            ..Default::default()
        };
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .with_resources(resources)
            .build()
            .await
            .unwrap();

        let id =
            persist_prompt_user_entry(&session, "/deploy service", &[], Some("rpc-x".into()), None)
                .await
                .expect("the origin path persists")
                .expect("an entry id comes back");
        let expanded =
            manox_harness::harness::expand_prompt_with(session.resources(), "/deploy service");
        assert_eq!(expanded, "deploy service now");
        let content_of = |text: &str| {
            serde_json::to_value(match prompt_user_message(text, &[]) {
                manox_harness::types::AgentMessage::User { content, .. } => content,
                _ => unreachable!("prompt_user_message builds a user message"),
            })
            .unwrap()
        };
        let appender = session.journal_appender();
        assert!(
            appender
                .take_accepted_user_entry(&content_of("/deploy service"))
                .is_none(),
            "the raw text must not match the pin"
        );
        assert_eq!(
            appender
                .take_accepted_user_entry(&content_of(&expanded))
                .as_deref(),
            Some(id.as_str()),
            "the expanded announce consumes the pin"
        );
    }

    /// Every on-disk `type` tag the replay-consistency regression must
    /// cover: the full state-change vocabulary the fold and restore
    /// rebuild consume, the transcript + lifecycle + delta kinds the
    /// display projection consumes, and the two kernel-written faces
    /// (`thinking_level_change`, `active_tools_change`). Tree-management
    /// kinds (`leaf`, `label`, `session_info`, `branch_summary`,
    /// `custom_message`) and the dedicated-path kinds (`message` is
    /// covered by the real turn; `compaction` owns a richer append path
    /// the typed face refuses by design) are out of the scripted set.
    const REPLAY_COVERAGE_KINDS: &[&str] = &[
        "message",
        "turn_start",
        "turn_finish",
        "stop",
        "retry",
        "error",
        "agent_text_delta",
        "agent_thinking_delta",
        "tool_call",
        "tool_result",
        "tool_output_chunk",
        "subagent_child",
        "subagent_progress",
        "model_change",
        "cwd_change",
        "project_change",
        "permission_mode_change",
        "thinking_level_change",
        "plan_mode_change",
        "plan_update",
        "plan_review",
        "goal",
        "title",
        "browser_suites",
        "background_task",
        "approval",
        "pinned_archived",
        "compaction_started",
        "metrics",
        "custom",
        "active_tools_change",
    ];

    fn entry_type_tag(entry: &manox_harness::session::SessionTreeEntry) -> String {
        serde_json::to_value(entry)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(|tag| tag.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    /// K1 (L10) replay-consistency gate: a scripted session covering every
    /// replay-supported journal kind — the live state (replay fold, restore
    /// rebuild, display projection, cursor, transcript) must equal, field
    /// by field, the state rebuilt from the on-disk file through the
    /// production reload path (`builder.open`, the same seam `run_actor`'s
    /// restore uses). Timestamps inside message payloads are the one
    /// as-built exception (K5): the journal is the authority and they are
    /// not a byte-for-byte assertion face, so the transcript comparison
    /// strips them.
    #[tokio::test]
    async fn journal_replay_is_consistent_across_disk_reload() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let sessions = dir.path().join("sessions");

        let stream = Arc::new(ToolRoundsStream {
            rounds: 0, // first call answers "done": the turn completes
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let resolver_for = |stream: Arc<ToolRoundsStream>| {
            let resolver: manox_harness::agent_loop::StreamResolver =
                Arc::new(move |_m: &PiModel| {
                    Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
                });
            resolver
        };
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(sessions.clone())
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver_for(Arc::clone(&stream))))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // A real turn: the user + assistant `message` entries land through
        // the persistence middleware (the deferred session materializes).
        session.prompt("replay consistency turn").await.unwrap();

        // One entry per supported kind through the production typed-append
        // face — the same `append_typed` the serializer runs.
        let appender = session.journal_appender();
        for (kind, payload) in scripted_state_rows() {
            appender
                .append_typed(kind, payload)
                .await
                .unwrap_or_else(|err| panic!("typed append `{kind}` must land: {err:#}"));
        }
        // The kernel-written faces: the reasoning-effort entry in its
        // on-disk vocabulary (the `set_thinking_level` clamp against a
        // non-thinking test model would record "off", which is not an
        // effort — the clamp is kernel-owned and harness-tested) and the
        // active-tool set through the production kernel face.
        appender
            .append_typed(
                "thinking_level_change",
                serde_json::json!({ "thinkingLevel": "max" }),
            )
            .await
            .unwrap();
        session
            .set_active_tools(vec!["Read".into(), "Grep".into()])
            .await
            .unwrap();
        // A UI annotation card (the display projection's `custom` face) —
        // appended through the same storage face persist_ui_note uses (the
        // resilient wrapper needs the actor's state/notice sink, which this
        // storage-level replay test does not run).
        let note_record = UiNoteRecord {
            kind: crate::db::UiNoteKind::Notice,
            data: serde_json::json!({ "text": "scripted note" }),
        };
        session
            .append_custom(UI_NOTE_CUSTOM_TYPE, serde_json::to_value(&note_record).ok())
            .await
            .unwrap();

        // ── Live-state capture. ──────────────────────────────────────────
        let live_records = appender.storage().journal_range(0, u64::MAX).await.unwrap();
        let live_replay = crate::replay::replay_thread_state(&live_records);
        let live_rebuilt = rebuild_restored_state(&session, &sessions).await;
        let live_entries = session.context_entries().await.unwrap();
        let (live_display, live_notes) = adapt::entries_to_display(&live_entries);
        let live_cursor = appender.storage().journal_cursor().await;
        let strip_timestamps = |messages: &[AgentMessage]| -> Vec<serde_json::Value> {
            messages
                .iter()
                .map(|message| {
                    let mut value = serde_json::to_value(message).unwrap();
                    if let Some(object) = value.as_object_mut() {
                        object.remove("timestamp");
                    }
                    value
                })
                .collect()
        };
        let live_transcript = strip_timestamps(session.harness_messages());
        let live_path = session.path().to_path_buf();

        // The fold reflects the scripted decisions (the entries are the
        // authority — not empty defaults), last-wins per field.
        assert_eq!(live_replay.title.as_deref(), Some("replayed title"));
        assert_eq!(live_replay.pinned, Some(false));
        assert_eq!(live_replay.archived, Some(true));
        assert_eq!(live_replay.project, Some(Some("/replay/proj".into())));
        assert_eq!(
            live_replay.permission_mode,
            Some(PermissionMode::DangerFullAccess)
        );
        assert_eq!(live_replay.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(live_replay.plan_mode, Some(true));
        assert_eq!(live_replay.cwd.as_deref(), Some("/replay/work"));
        assert_eq!(
            live_replay.goal,
            Some(serde_json::json!({ "objective": "replay" }))
        );
        assert_eq!(
            live_rebuilt.title.as_deref(),
            Some("replayed title"),
            "restore rebuild must read the journal title"
        );
        assert!(!live_rebuilt.pinned && live_rebuilt.archived);
        assert_eq!(live_rebuilt.project, Some(PathBuf::from("/replay/proj")));
        assert_eq!(
            live_rebuilt.permission_mode,
            PermissionMode::DangerFullAccess
        );
        assert_eq!(live_rebuilt.reasoning_effort, ReasoningEffort::Max);
        assert!(live_rebuilt.plan_mode);

        // K2 cache repair: the diverging (here: empty) sidecar converges
        // toward the journal authority — INCLUDING the title (the repair
        // is enabled by the rename-route decision: a pi thread's sidecar
        // title is written only by the journaled auto-title scheduler; the
        // direct writer `set_external_title` serves external TUI sessions,
        // which carry no pi chain and never reach this rebuild).
        let repaired = manox_harness::session_meta::load(&sessions, &live_path)
            .await
            .unwrap();
        assert_eq!(repaired.title.as_deref(), Some("replayed title"));
        assert!(!repaired.pinned && repaired.archived);
        assert_eq!(repaired.project.as_deref(), Some("/replay/proj"));
        assert_eq!(
            repaired.approval_mode.as_deref(),
            Some("danger-full-access")
        );
        assert_eq!(repaired.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(repaired.plan_mode, Some(true));

        // Coverage guard: every supported kind is actually on the chain —
        // a silently skipped kind would make the round-trip assertions
        // vacuous.
        let live_tags: std::collections::HashSet<String> = live_records
            .iter()
            .map(|record| entry_type_tag(&record.entry))
            .collect();
        for kind in REPLAY_COVERAGE_KINDS {
            assert!(
                live_tags.contains(*kind),
                "the scripted session must cover `{kind}`; on chain: {live_tags:?}"
            );
        }

        // ── Reload through the production path and re-capture. ──────────
        session.close().await.unwrap();
        let reloaded = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(sessions.clone())
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver_for(Arc::clone(&stream))))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .open(live_path)
            .await
            .unwrap();
        let reloaded_records = reloaded.journal_range(0, u64::MAX).await.unwrap();
        let reloaded_replay = crate::replay::replay_thread_state(&reloaded_records);
        let reloaded_rebuilt = rebuild_restored_state(&reloaded, &sessions).await;
        let reloaded_entries = reloaded.context_entries().await.unwrap();
        let (reloaded_display, reloaded_notes) = adapt::entries_to_display(&reloaded_entries);
        let reloaded_cursor = reloaded.journal_cursor().await;
        let reloaded_transcript = strip_timestamps(reloaded.harness_messages());

        // ── Disk reload == live memory, field by field (§J.2). ──────────
        assert_eq!(
            live_replay, reloaded_replay,
            "the replay fold diverged across the disk round trip"
        );
        assert_eq!(
            live_rebuilt, reloaded_rebuilt,
            "the restore rebuild diverged across the disk round trip"
        );
        assert_eq!(
            live_records.len(),
            reloaded_records.len(),
            "the reloaded chain lost or gained entries"
        );
        for (live, reloaded) in live_records.iter().zip(&reloaded_records) {
            assert_eq!(live.seq, reloaded.seq, "seq diverged on the round trip");
            assert_eq!(
                serde_json::to_value(&live.entry).unwrap(),
                serde_json::to_value(&reloaded.entry).unwrap(),
                "entry `{}` diverged on the round trip",
                entry_type_tag(&live.entry)
            );
        }
        // The display comparison strips the per-message `id`: the mapping
        // mints a fresh UUID on every rebuild (it is not journal-derived),
        // so like message-payload timestamps (the K5 as-built note) it is
        // not a byte-for-byte assertion face. Everything else — roles,
        // content, note cards, ordering — must round-trip exactly.
        let display_shape = |display: &[HistoryEntry]| -> Vec<serde_json::Value> {
            display
                .iter()
                .map(|entry| {
                    let mut value = serde_json::to_value(entry).unwrap();
                    if let Some(message) = value.get_mut("Message").and_then(|m| m.as_object_mut())
                    {
                        message.remove("id");
                    }
                    value
                })
                .collect()
        };
        assert_eq!(
            display_shape(&live_display),
            display_shape(&reloaded_display),
            "the display projection diverged across the disk round trip"
        );
        let note_shape = |notes: &[crate::db::PositionedNote]| -> Vec<serde_json::Value> {
            notes
                .iter()
                .map(|note| {
                    serde_json::json!({
                        "afterMessage": note.after_message,
                        "note": serde_json::to_value(&note.note).unwrap(),
                    })
                })
                .collect()
        };
        assert_eq!(
            note_shape(&live_notes),
            note_shape(&reloaded_notes),
            "the UI-note positions diverged across the disk round trip"
        );
        assert_eq!(
            live_cursor, reloaded_cursor,
            "the journal cursor diverged across the disk round trip"
        );
        assert_eq!(
            live_transcript, reloaded_transcript,
            "the transcript diverged across the disk round trip"
        );
        // The reloaded chain stays dense (L4): contiguous seq from 0 and
        // the cursor sits on the last record.
        for (index, record) in reloaded_records.iter().enumerate() {
            assert_eq!(record.seq, index as u64, "the reloaded chain is not dense");
        }
        assert_eq!(
            reloaded_cursor,
            reloaded_records
                .last()
                .map(|record| record.seq)
                .unwrap_or(0)
        );
    }

    /// K2 authority: the journal rebuild WINS over a diverging sidecar, and
    /// the sidecar cache is repaired toward the journal.
    #[tokio::test]
    async fn restored_state_prefers_journal_over_sidecar_and_repairs_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let sessions = dir.path().join("sessions");
        let stream = Arc::new(ToolRoundsStream {
            rounds: 0,
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(sessions.clone())
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        // A stale sidecar: the cache says one thing …
        manox_harness::session_meta::update(&sessions, session.path(), |meta| {
            meta.title = Some("stale sidecar title".into());
            meta.pinned = true;
            meta.archived = false;
            meta.approval_mode = Some("read-only".into());
            meta.project = Some("/stale/project".into());
        })
        .await
        .unwrap();
        // … the journal says another (K3's decision-point entries).
        let appender = session.journal_appender();
        appender
            .append_typed("title", serde_json::json!({ "title": "journal title" }))
            .await
            .unwrap();
        appender
            .append_typed(
                "pinned_archived",
                serde_json::json!({ "pinned": false, "archived": true }),
            )
            .await
            .unwrap();
        appender
            .append_typed(
                "permission_mode_change",
                serde_json::json!({ "mode": "workspace-write" }),
            )
            .await
            .unwrap();
        appender
            .append_typed(
                "project_change",
                serde_json::json!({ "path": "/journal/project" }),
            )
            .await
            .unwrap();
        appender
            .append_typed(
                "goal",
                serde_json::json!({ "goal": { "objective": "restored goal" } }),
            )
            .await
            .unwrap();
        appender
            .append_typed(
                "plan_review",
                serde_json::json!({ "state": "proposed", "planFile": null }),
            )
            .await
            .unwrap();

        let rebuilt = rebuild_restored_state(&session, &sessions).await;
        assert_eq!(rebuilt.title.as_deref(), Some("journal title"));
        assert!(!rebuilt.pinned);
        assert!(rebuilt.archived);
        assert_eq!(rebuilt.permission_mode, PermissionMode::WorkspaceWrite);
        assert_eq!(rebuilt.project, Some(PathBuf::from("/journal/project")));
        // Goal stage ②: the rebuild passes the replayed journal snapshot
        // through to the Ready chain (the bridge seed's authority).
        assert_eq!(
            rebuilt
                .goal
                .as_ref()
                .and_then(|g| g.get("objective"))
                .and_then(|o| o.as_str()),
            Some("restored goal")
        );
        // Plan-review C4 vocabulary: the chain's proposed edge is the fold
        // source — the sidecar carries no flag, so a sidecar-only merge
        // would answer false.
        assert!(
            rebuilt.plan_review_pending,
            "the journal plan_review edge must fold the pending flag"
        );

        // The cache converged toward the authority in the same pass —
        // the title INCLUDED (the rename-route decision: the pi-thread
        // sidecar title's only writer is the journaled auto-title
        // scheduler, so a divergence is a stale cache, not a newer user
        // decision; external-session titles never reach this rebuild).
        let repaired = manox_harness::session_meta::load(&sessions, session.path())
            .await
            .unwrap();
        assert_eq!(repaired.title.as_deref(), Some("journal title"));
        assert!(!repaired.pinned && repaired.archived);
        assert_eq!(repaired.approval_mode.as_deref(), Some("workspace-write"));
        assert_eq!(repaired.project.as_deref(), Some("/journal/project"));
        session.close().await.unwrap();
    }

    /// K2 migration window: a legacy chain without decision-point entries
    /// resolves entirely from the sidecar — the rebuild never overwrites
    /// cached state with defaults.
    #[tokio::test]
    async fn restored_state_falls_back_to_sidecar_for_legacy_chains() {
        let replayed = crate::replay::ReplayedThreadState::default();
        let meta = manox_harness::session_meta::SessionMeta {
            title: Some("sidecar title".into()),
            project: Some("/sidecar/project".into()),
            approval_mode: Some("read-only".into()),
            plan_mode: Some(true),
            reasoning_effort: Some("max".into()),
            plan_file: Some("/plans/x-plan.md".into()),
            plan_review_pending: Some(true),
            plan_snapshot: Some(serde_json::json!([{ "content": "step" }])),
            pinned: true,
            archived: true,
            ..Default::default()
        };
        let merged = merge_restored_state(&replayed, &meta);
        assert_eq!(merged.title.as_deref(), Some("sidecar title"));
        assert_eq!(merged.project, Some(PathBuf::from("/sidecar/project")));
        assert_eq!(merged.permission_mode, PermissionMode::ReadOnly);
        assert!(merged.plan_mode);
        assert_eq!(merged.reasoning_effort, ReasoningEffort::Max);
        assert_eq!(merged.plan_file.as_deref(), Some("/plans/x-plan.md"));
        assert!(merged.plan_review_pending);
        assert_eq!(
            merged.plan_snapshot,
            Some(serde_json::json!([{ "content": "step" }]))
        );
        assert!(merged.pinned && merged.archived);
    }

    /// A cleared plan (the empty `plan_update` snapshot the facade
    /// persists on clear) normalizes to the sidecar's absence semantics —
    /// the rebuild must not resurrect an empty plan rail.
    #[test]
    fn cleared_plan_snapshot_normalizes_to_absence() {
        let replayed = crate::replay::ReplayedThreadState {
            plan_snapshot: Some(serde_json::json!([])),
            ..Default::default()
        };
        let meta = manox_harness::session_meta::SessionMeta::default();
        let merged = merge_restored_state(&replayed, &meta);
        assert_eq!(merged.plan_snapshot, None);
    }

    /// K3 routing: a store-level decision row reaches the live actor's
    /// command queue through the registry route, and the shutdown claim
    /// lands queued rows in the journal even when they arrive behind the
    /// actor's `Shutdown` break (the gateway archives right after dispose).
    #[tokio::test]
    async fn store_journal_rows_route_to_the_actor_and_the_shutdown_claim_lands_them() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let thread_id = format!("route-test-{}", uuid::Uuid::new_v4());

        let stream = Arc::new(ToolRoundsStream {
            rounds: 0,
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
        register_engine_route(&thread_id, &cmd_tx);

        // Live route: the dispatch lands on the actor queue (send order =
        // persist order, §C.3).
        dispatch_store_journal_row(
            thread_id.clone(),
            None,
            "pinned_archived".into(),
            serde_json::json!({ "pinned": true, "archived": false }),
        );
        let queued = tokio::time::timeout(std::time::Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("the routed row must reach the actor queue")
            .expect("the channel stays open");
        match queued {
            SessionCmd::AppendJournal { kind, payload } => {
                assert_eq!(kind, "pinned_archived");
                assert_eq!(payload["pinned"], serde_json::json!(true));
            }
            _other => panic!("expected an AppendJournal row, got a different SessionCmd variant"),
        }

        // Shutdown claim: a row queued while the actor is exiting is
        // claimed under the registry lock and appended before close.
        let appender = session.journal_appender();
        dispatch_store_journal_row(
            thread_id.clone(),
            None,
            "pinned_archived".into(),
            serde_json::json!({ "pinned": false, "archived": true }),
        );
        let claimed = retire_and_claim_journal_rows(&thread_id, &mut cmd_rx);
        assert_eq!(claimed.len(), 1, "the queued row must be claimed");
        for (kind, payload) in claimed {
            append_row_fail_loud(&appender, kind, payload).await;
        }
        let rows = appender.storage().journal_range(0, u64::MAX).await.unwrap();
        assert!(
            rows.iter().any(|record| matches!(
                &record.entry,
                manox_harness::session::SessionTreeEntry::PinnedArchived { pinned, archived, .. }
                    if !*pinned && *archived
            )),
            "the claimed row must land in the journal"
        );

        // A retired route never accepts new rows: the dispatch waits for
        // the route's removal, then takes the cold path (no file here —
        // the row is skipped, which the wait+removal must not hang on).
        unregister_engine_route(&thread_id, &cmd_tx);
        dispatch_store_journal_row(
            thread_id.clone(),
            None,
            "pinned_archived".into(),
            serde_json::json!({ "pinned": true, "archived": true }),
        );
        // Give the spawned waiter a beat; the assertion is that the route
        // table stays clean (the test's global-state hygiene) and nothing
        // panics or hangs.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            engine_routes().lock().unwrap().get(&thread_id).is_none(),
            "the test must leave the engine-route registry clean"
        );
        session.close().await.unwrap();
    }

    /// A minimal session for the K3 decision-point tests: a real jsonl
    /// storage (deferred until first write), a scripted stream that is
    /// never exercised, and the production builder path.
    async fn decision_rig_session(dir: &tempfile::TempDir) -> AgentSession {
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let stream = Arc::new(ToolRoundsStream {
            rounds: 0,
            call: std::sync::atomic::AtomicUsize::new(0),
        });
        let resolver: manox_harness::agent_loop::StreamResolver = Arc::new(move |_m: &PiModel| {
            Ok(Arc::clone(&stream) as Arc<dyn manox_harness::agent_loop::StreamFn>)
        });
        create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(ModelRuntime::new(resolver))
            .with_model(test_model())
            .with_system_prompt("You are a test assistant.")
            .build()
            .await
            .unwrap()
    }

    /// K3 (L3) regression: the project-binding decision journals a
    /// `project_change` entry alongside the sidecar cache write — pre-fix
    /// the binding was written to the sidecar only, leaving the chain
    /// without the authority K2 rebuilds from.
    #[tokio::test]
    async fn project_binding_journals_project_change_entry() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session = decision_rig_session(&dir).await;
        let state = test_engine_state();
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

        bind_project(
            &sessions,
            &session,
            Path::new("/bound/project"),
            &state,
            &notice_tx,
        )
        .await;

        let rows = session.journal_range(0, u64::MAX).await.unwrap();
        assert!(
            rows.iter().any(|record| matches!(
                &record.entry,
                manox_harness::session::SessionTreeEntry::ProjectChange { path, .. }
                    if path.as_deref() == Some("/bound/project")
            )),
            "the binding must journal a project_change entry; chain kinds: {:?}",
            rows.iter()
                .map(|record| entry_type_tag(&record.entry))
                .collect::<Vec<_>>()
        );
        // The sidecar cache follows (fast-list mirror, K2).
        let meta = manox_harness::session_meta::load(&sessions, session.path())
            .await
            .unwrap();
        assert_eq!(meta.project.as_deref(), Some("/bound/project"));
        session.close().await.unwrap();
    }

    /// K3 (L3) regression: the permission-mode decision emits the notice
    /// whose tap mapping journals `permission_mode_change` — pre-fix the
    /// choice landed in the gate and the sidecar only, so the chain never
    /// carried the user's mode toggle.
    #[tokio::test]
    async fn permission_mode_decision_journals_through_the_tap() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session = decision_rig_session(&dir).await;
        let state = test_engine_state();
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

        apply_permission_mode(
            &state,
            &sessions,
            session.path(),
            PermissionMode::ReadOnly,
            &notice_tx,
        )
        .await;

        assert_eq!(state.gate.mode(), PermissionMode::ReadOnly);
        let meta = manox_harness::session_meta::load(&sessions, session.path())
            .await
            .unwrap();
        assert_eq!(meta.approval_mode.as_deref(), Some("read-only"));

        // The decision notifies, and the tap's mapping of that notice is
        // the journal row (the same `durable_journal_payload` face the
        // spawn_engine tap runs). Bounded: a missing emission must fail
        // the test, not hang it.
        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), notice_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("the mode decision must reach the notice face"))
            .expect("the notice channel must stay open");
        let BackendNotice::Event(event) = notice else {
            panic!("the mode decision must ride a ThreadEvent notice");
        };
        let ThreadEvent::PermissionModeChanged { mode } = *event else {
            panic!("expected PermissionModeChanged");
        };
        assert_eq!(mode, PermissionMode::ReadOnly);
        let (kind, payload) = durable_journal_payload(&ThreadEvent::PermissionModeChanged { mode })
            .expect("the tap maps the decision to a typed row");
        assert_eq!(kind, "permission_mode_change");
        session
            .journal_appender()
            .append_typed(&kind, payload)
            .await
            .unwrap();
        let rows = session.journal_range(0, u64::MAX).await.unwrap();
        assert!(
            rows.iter().any(|record| matches!(
                &record.entry,
                manox_harness::session::SessionTreeEntry::PermissionModeChange { mode, .. }
                    if mode == "read-only"
            )),
            "the decision must land a permission_mode_change entry"
        );
        session.close().await.unwrap();
    }

    /// K3 (L3) regression: the initial-title decision rides the same
    /// notice tap as the generated title — pre-fix only
    /// `SessionListDirty` fired, so the chain never carried a `title`
    /// entry for the initial title and the K2 rebuild stayed blind to it.
    #[tokio::test]
    async fn initial_title_decision_journals_through_the_tap() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session = decision_rig_session(&dir).await;
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<BackendNotice>();

        persist_initial_title(
            &sessions,
            session.path(),
            "initial title".to_string(),
            &notice_tx,
        )
        .await;

        let meta = manox_harness::session_meta::load(&sessions, session.path())
            .await
            .unwrap();
        assert_eq!(meta.title.as_deref(), Some("initial title"));

        // First the sidebar refresh, then the TitleChanged notice whose
        // tap mapping is the `title` row. Bounded: a missing emission must
        // fail the test, not hang it.
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), notice_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("the sidebar must refresh"))
            .expect("the notice channel must stay open");
        assert!(
            matches!(first, BackendNotice::SessionListDirty),
            "the refresh notice comes first"
        );
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), notice_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("the title decision must reach the notice face"))
            .expect("the notice channel must stay open");
        let BackendNotice::Event(event) = second else {
            panic!("the title decision must ride a ThreadEvent notice");
        };
        let ThreadEvent::TitleChanged { title } = *event else {
            panic!("expected TitleChanged");
        };
        assert_eq!(title, "initial title");
        let (kind, payload) = durable_journal_payload(&ThreadEvent::TitleChanged { title })
            .expect("the tap maps the decision to a typed row");
        assert_eq!(kind, "title");
        session
            .journal_appender()
            .append_typed(&kind, payload)
            .await
            .unwrap();
        let rows = session.journal_range(0, u64::MAX).await.unwrap();
        assert!(
            rows.iter().any(|record| matches!(
                &record.entry,
                manox_harness::session::SessionTreeEntry::Title { title, .. }
                    if title == "initial title"
            )),
            "the decision must land a title entry"
        );
        session.close().await.unwrap();
    }
}
